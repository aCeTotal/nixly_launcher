/// Filesystem walkers yield to the UI.
pub fn demote() {
    let param = libc::sched_param { sched_priority: 0 };
    unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) };
}
