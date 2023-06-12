use core::{future::Future, mem::size_of};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use arch::{get_time, trap_pre_handle, user_restore, Context, ContextOps, VirtPage};
use devices::NET_DEVICES;
use executor::{
    current_task, current_user_task, thread, yield_now, AsyncTask, Executor, KernelTask, MemType,
    UserTask, TASK_QUEUE,
};
use fs::socket::NetType;
use log::{debug, warn};
use lose_net_stack::{results::Packet, IPv4, LoseStack, MacAddress, TcpFlags};
use signal::SignalFlags;

use crate::syscall::{
    consts::{SignalUserContext, UserRef},
    exec_with_process, syscall, PORT_TABLE,
};

use self::initproc::initproc;

mod async_ops;
pub mod elf;
mod initproc;

pub use async_ops::{futex_requeue, futex_wake, NextTick, WaitFutex, WaitPid, WaitSignal};

#[no_mangle]
// for avoiding the rust cycle check. use extern and nomangle
pub fn user_entry() -> Box<dyn Future<Output = ()> + Send + Sync> {
    Box::new(async { user_entry_inner().await })
}

enum UserTaskControlFlow {
    Continue,
    Break,
}

async fn handle_syscall(task: Arc<UserTask>, cx_ref: &mut Context) -> UserTaskControlFlow {
    let ustart = 0;
    unsafe {
        user_restore(cx_ref);
    }
    task.inner_map(|inner| inner.tms.utime += (get_time() - ustart) as u64);

    let sstart = 0;
    let trap_type = trap_pre_handle(cx_ref);
    match trap_type {
        arch::TrapType::Breakpoint => {}
        arch::TrapType::UserEnvCall => {
            debug!("user env call: {}", cx_ref.syscall_number());
            // if syscall ok
            let args = cx_ref.args();
            let args = [
                args[0], args[1], args[2], args[3], args[4], args[5], args[6],
            ];
            let call_number = cx_ref.syscall_number();
            cx_ref.syscall_ok();
            let result = syscall(call_number, args)
                .await
                .map_or_else(|e| -e.code(), |x| x as isize) as usize;
            debug!("syscall result: {:#X?}", result);
            cx_ref.set_ret(result);
            if result == (-500 as isize) as usize {
                return UserTaskControlFlow::Break;
            }
        }
        arch::TrapType::Time => {
            debug!("time interrupt from user");
        }
        arch::TrapType::Unknown => {
            debug!("unknown trap: {:#x?}", cx_ref);
            panic!("");
        }
        arch::TrapType::StorePageFault(addr) => {
            let vpn = VirtPage::from_addr(addr);
            warn!("store page fault @ {:#x}", addr);

            let finded = task
                .pcb
                .lock()
                .memset
                .iter()
                .find_map(|mem_area| {
                    mem_area
                        .mtrackers
                        .iter()
                        .find(|x| x.vpn == vpn && mem_area.mtype == MemType::Clone)
                })
                .cloned();

            warn!("store page: {:?}", finded);

            match finded {
                Some(_) => {
                    // let src_ppn = tracker.0;
                    // let dst_ppn = task.frame_alloc(vpn, MemType::CodeSection);
                    // dst_ppn.copy_value_from_another(src_ppn);
                }
                None => {
                    warn!("alloc judge addr: {:#x}", addr);
                    if (0x7ff00000..0x7ffff000).contains(&addr) {
                        warn!("alloc");
                        task.frame_alloc(vpn, MemType::Stack, 1);
                        warn!("alloc page: {:#x}", addr);
                    } else {
                        debug!("context: {:#X?}", cx_ref);
                        return UserTaskControlFlow::Break;
                    }
                }
            }
        }
    }
    task.inner_map(|inner| inner.tms.stime += (get_time() - sstart) as u64);
    UserTaskControlFlow::Continue
}

pub async fn handle_signal(task: Arc<UserTask>, signal: SignalFlags) {
    debug!(
        "handle signal: {:?} task_id: {}",
        signal,
        task.get_task_id()
    );

    let sigaction = task.pcb.lock().sigaction[signal.num()].clone();

    if sigaction.handler == 0 {
        match signal {
            SignalFlags::SIGCANCEL => {
                current_user_task().exit_with_signal(signal.num());
            }
            _ => {}
        }
        return;
    }
    let proc_mask = task.tcb.read().sigmask;
    task.tcb.write().sigmask = sigaction.mask;

    let cx_ref = unsafe { task.get_cx_ptr().as_mut().unwrap() };

    let mut sp = cx_ref.sp();

    sp -= 128;
    sp -= size_of::<SignalUserContext>();
    sp = sp / 16 * 16;

    let cx = UserRef::<SignalUserContext>::from(sp).get_mut();
    let store_cx = cx_ref.clone();

    let mut tcb = task.tcb.write();
    cx.pc = tcb.cx.sepc();
    cx.sig_mask = sigaction.mask;
    // debug!("pc: {:#X}, mask: {:#X?}", cx.pc, cx.sig_mask);
    tcb.cx.set_sepc(sigaction.handler);
    tcb.cx.set_ra(sigaction.restorer);
    tcb.cx.set_arg0(signal.num());
    tcb.cx.set_arg1(0);
    tcb.cx.set_arg2(sp);
    drop(tcb);

    loop {
        if let Some(exit_code) = task.exit_code() {
            debug!("program exit with code: {}", exit_code);
            break;
        }

        if let UserTaskControlFlow::Break = handle_syscall(task.clone(), cx_ref).await {
            break;
        }
    }
    task.tcb.write().sigmask = proc_mask;
    // debug!("new pc: {:#X}", cx.pc);
    // store_cx.set_ret(cx_ref.args()[0]);
    cx_ref.clone_from(&store_cx);
    // copy pc from new_pc
    cx_ref.set_sepc(cx.pc);
}

pub async fn user_entry_inner() {
    let mut times = 0;
    loop {
        let task = current_user_task();
        debug!("task: {}", task.get_task_id());
        loop {
            let signal = task.tcb.read().signal.try_get_signal();
            if let Some(signal) = signal {
                handle_signal(task.clone(), signal.clone()).await;
                task.tcb.write().signal.remove_signal(signal);
            } else {
                break;
            }
        }
        let cx_ref = unsafe { task.get_cx_ptr().as_mut().unwrap() };
        debug!(
            "user_entry, task: {}, sepc: {:#X}",
            task.task_id,
            cx_ref.sepc()
        );

        if let UserTaskControlFlow::Break = handle_syscall(task.clone(), cx_ref).await {
            break;
        }

        if let Some(exit_code) = task.exit_code() {
            debug!("program exit with code: {}", exit_code);
            break;
        }

        times += 1;

        if times >= 50 {
            times = 0;
            yield_now().await;
        }

        // yield_now().await;
    }
    debug!("exit_task: {}", current_task().get_task_id());
}

#[allow(dead_code)]
pub async fn handle_net() {
    let lose_stack = LoseStack::new(
        IPv4::new(10, 0, 2, 15),
        MacAddress::new([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
    );

    let mut buffer = vec![0u8; 2048];
    loop {
        if TASK_QUEUE.lock().len() == 1 {
            break;
        }
        let rlen = NET_DEVICES.lock()[0].recv(&mut buffer).unwrap_or(0);
        if rlen != 0 {
            let packet = lose_stack.analysis(&buffer[..rlen]);
            debug!("packet: {:?}", packet);
            match packet {
                Packet::ARP(arp_packet) => {
                    debug!("receive arp packet: {:?}", arp_packet);
                    let reply_packet = arp_packet
                        .reply_packet(lose_stack.ip, lose_stack.mac)
                        .expect("can't build reply");
                    NET_DEVICES.lock()[0]
                        .send(&reply_packet.build_data())
                        .expect("can't send net data");
                }
                Packet::UDP(udp_packet) => {
                    debug!("udp_packet: {:?}", udp_packet);
                }
                Packet::TCP(tcp_packet) => {
                    let net = NET_DEVICES.lock()[0].clone();
                    if tcp_packet.flags == TcpFlags::S {
                        // receive a tcp connect packet
                        let mut reply_packet = tcp_packet.ack();
                        reply_packet.flags = TcpFlags::S | TcpFlags::A;
                        if let Some(socket) = PORT_TABLE.lock().get(&tcp_packet.dest_port) {
                            // TODO: create a new socket as the child of this socket.
                            // and this is receive a child.
                            // TODO: specific whether it is tcp or udp

                            info!(
                                "[TCP CONNECT]{}:{}(MAC:{}) -> {}:{}(MAC:{})  len:{}",
                                tcp_packet.source_ip,
                                tcp_packet.source_port,
                                tcp_packet.source_mac,
                                tcp_packet.dest_ip,
                                tcp_packet.dest_port,
                                tcp_packet.dest_mac,
                                tcp_packet.data_len
                            );
                            if socket.net_type == NetType::STEAM {
                                socket.add_wait_queue(
                                    tcp_packet.source_ip.to_u32(),
                                    tcp_packet.source_port,
                                );
                                let reply_data = &reply_packet.build_data();
                                net.send(&reply_data).expect("can't send to net");
                            }
                        }
                    } else if tcp_packet.flags.contains(TcpFlags::F) {
                        // tcp disconnected
                        info!(
                            "[TCP DISCONNECTED]{}:{}(MAC:{}) -> {}:{}(MAC:{})  len:{}",
                            tcp_packet.source_ip,
                            tcp_packet.source_port,
                            tcp_packet.source_mac,
                            tcp_packet.dest_ip,
                            tcp_packet.dest_port,
                            tcp_packet.dest_mac,
                            tcp_packet.data_len
                        );
                        let reply_packet = tcp_packet.ack();
                        net.send(&reply_packet.build_data())
                            .expect("can't send to net");

                        let mut end_packet = reply_packet.ack();
                        end_packet.flags |= TcpFlags::F;
                        net.send(&end_packet.build_data())
                            .expect("can't send to net");
                    } else {
                        info!(
                            "{}:{}(MAC:{}) -> {}:{}(MAC:{})  len:{}",
                            tcp_packet.source_ip,
                            tcp_packet.source_port,
                            tcp_packet.source_mac,
                            tcp_packet.dest_ip,
                            tcp_packet.dest_port,
                            tcp_packet.dest_mac,
                            tcp_packet.data_len
                        );

                        if tcp_packet.flags.contains(TcpFlags::A) && tcp_packet.data_len == 0 {
                            continue;
                        }

                        if let Some(socket) = PORT_TABLE.lock().get(&tcp_packet.dest_port) {
                            let socket_inner = socket.inner.lock();
                            let client = socket_inner.clients.iter().find(|x| match x.upgrade() {
                                Some(x) => {
                                    let client_inner = x.inner.lock();
                                    client_inner.target_ip == tcp_packet.source_ip.to_u32()
                                        && client_inner.target_port == tcp_packet.source_port
                                }
                                None => false,
                            });

                            client.map(|x| {
                                let socket = x.upgrade().unwrap();
                                let mut socket_inner = socket.inner.lock();

                                socket_inner.datas.push_back(tcp_packet.data.to_vec());
                                let reply = tcp_packet.reply(&[0u8; 0]);
                                socket_inner.ack = reply.ack;
                                socket_inner.seq = reply.seq;
                                socket_inner.flags = reply.flags.bits();
                            });
                        }

                        // handle tcp data
                        // receive_tcp(&mut net, &tcp_packet)
                    }
                }
                Packet::ICMP() => {}
                Packet::IGMP() => todo!(),
                Packet::Todo(_) => todo!(),
                Packet::None => todo!(),
            }
        }
        yield_now().await;
    }
}

pub fn init() {
    let mut exec = Executor::new();
    exec.spawn(KernelTask::new(initproc()));
    #[cfg(feature = "net")]
    exec.spawn(KernelTask::new(handle_net()));
    // exec.spawn()
    exec.run();
}

pub async fn add_user_task(filename: &str, args: Vec<&str>, _envp: Vec<&str>) {
    let task = UserTask::new(user_entry(), Arc::downgrade(&current_task()));
    exec_with_process(task.clone(), filename, args).expect("can't add task to excutor");
    thread::spawn(task.clone());
}
