extern "C" {
    #[link_name = "SpinLockInit__pgrx_cshim"]
    pub fn SpinLockInit(lock: *mut slock_t);
    #[link_name = "SpinLockAcquire__pgrx_cshim"]
    pub fn SpinLockAcquire(lock: *mut slock_t);
    #[link_name = "SpinLockRelease__pgrx_cshim"]
    pub fn SpinLockRelease(lock: *mut slock_t);
    #[link_name = "SpinLockFree__pgrx_cshim"]
    pub fn SpinLockFree(lock: *mut slock_t) -> bool;
}
