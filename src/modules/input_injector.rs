use std::process::abort;

use libafl::{inputs::HasTargetBytes, HasMetadata};
use libafl_qemu::{
    modules::{utils::filters::NopAddressFilter, EmulatorModule, EmulatorModuleTuple}, EmulatorModules, GuestAddr, Hook, Qemu, SYS_exit, SYS_exit_group, SYS_mmap, SYS_munmap, SYS_read, SyscallHookResult
};

use crate::modules::ExecMeta;

#[derive(Default, Debug)]
pub struct InputInjectorModule {
    // Save the Mutator's BytesInput
    input: Vec<u8>,
    input_addr: GuestAddr,
    max_size: usize,
}

impl InputInjectorModule {
    pub fn new() -> Self {
        Self {
            max_size: 1048576,
            ..Default::default()
        }
    }

    pub fn set_input_addr(&mut self, addr: GuestAddr) {
        self.input_addr = addr;
    }
}

impl<I, S> EmulatorModule<I, S> for InputInjectorModule
where
    S: Unpin + HasMetadata,
    I: Unpin + HasTargetBytes, 
{
    type ModuleAddressFilter = NopAddressFilter;

    fn first_exec<ET>(
        &mut self,
        _qemu: Qemu,
        _emulator_modules: &mut EmulatorModules<ET, I, S>,
        _state: &mut S,
    ) where
        ET: EmulatorModuleTuple<I, S>,
    {
        log::debug!("InputInjectorModule::first_exec running ...");

        if let Some(hook_id) =
            _emulator_modules.pre_syscalls(Hook::Function(syscall_hooks::<ET, I, S>))
        {
            log::debug!("Hook {:?} installed", hook_id);
        } else {
            log::error!("Failed to install hook");
        }

        let exec_meta = ExecMeta::new();
        _state.add_metadata(exec_meta);
    }

    fn pre_exec<ET>(
        &mut self,
        _qemu: Qemu,
        _emulator_modules: &mut EmulatorModules<ET, I, S>,
        _state: &mut S,
        _input: &I,
    ) where
        ET: EmulatorModuleTuple<I, S>,
    {   
        log::debug!("InputInjectorModule::pre_exec running ...");

        let mut tb = _input.target_bytes();
        if tb.len() > self.max_size {
            if let None = tb.truncate(self.max_size) {
                log::error!("Failed to truncate input");
                return;
            }
        }

        self.input.clear();
        self.input.extend_from_slice(&tb);

        // clean and fill the input_addr for further mmap usage
        let written_buf = if self.input.len() > self.max_size {
            &self.input[..self.max_size]
        } else {
            &self.input
        };
        _qemu.write_mem(self.input_addr, written_buf).unwrap();
    }

    fn address_filter(&self) -> &Self::ModuleAddressFilter {
        &NopAddressFilter
    }

    fn address_filter_mut(&mut self) -> &mut Self::ModuleAddressFilter {
        // unsafe { (&raw mut NOP_ADDRESS_FILTER).as_mut().unwrap().get_mut() }
        unimplemented!("This should never be called")
    }
}

/// This is user-defined syscall hook.
/// If create `SyscallHookResult` with `None`, the syscall will execute normally
/// If create `SyscallHookResult` with `Some(retval)`, the syscall will directly return the retval and not execute
fn syscall_hooks<ET, I, S>(
    _qemu: Qemu,
    emulator_modules: &mut EmulatorModules<ET, I, S>,
    _state: Option<&mut S>,
    sys_num: i32,
    a0: GuestAddr,
    a1: GuestAddr,
    _a2: GuestAddr,
    _a3: GuestAddr,
    _a4: GuestAddr,
    _a5: GuestAddr,
    _a6: GuestAddr,
    _a7: GuestAddr,
) -> SyscallHookResult
where
    S: Unpin + HasMetadata,
    I: Unpin + HasTargetBytes,
    ET: EmulatorModuleTuple<I, S>,
{
    let sys_num = sys_num as i64;
    // Hook syscall read
    if sys_num == SYS_read {
        log::debug!("Read syscall intercepted ...");
        let input_injector_module = emulator_modules
            .get_mut::<InputInjectorModule>()
            .expect("Failed to get InputInjectorModule");
        
        let input_len = input_injector_module.input.len();
        let offset: usize = if _a2 == 0 {
            0
        } else if _a2 as usize <= input_len {
            _a2.try_into().unwrap()
        } else {
            input_len
        };

        let drained = input_injector_module
            .input
            .drain(..offset)
            .as_slice().to_owned();

        _qemu.write_mem(a1, drained.as_slice()).unwrap();

        // Return the number of bytes read
        SyscallHookResult::new(Some(drained.len() as u64))
    }
    else if sys_num == SYS_mmap {
        if _a2 == 1 && _a3 == 1 {
            log::debug!("Mmap syscall intercepted ...");
            let input_injector_module = emulator_modules
                .get_mut::<InputInjectorModule>()
                .expect("Failed to get InputInjectorModule");
            log::debug!("Mmap return address: {:#x}", input_injector_module.input_addr);
            SyscallHookResult::new(Some(input_injector_module.input_addr))
        } else {
            SyscallHookResult::new(None)
        }
    }
    else if sys_num == SYS_munmap {
        let input_injector_module = emulator_modules
                .get_mut::<InputInjectorModule>()
                .expect("Failed to get InputInjectorModule");
        let addr = input_injector_module.input_addr;
        log::debug!("Munmap args: {:#x}, {:#x}", a0, a1);
        if a0 == addr {
            log::debug!("Munmap syscall intercepted ...");
            SyscallHookResult::new(Some(0))
        } else {
            SyscallHookResult::new(None)
        }
    }
    else if sys_num == SYS_exit || sys_num == SYS_exit_group {
        log::debug!("Exit / Exit group syscall intercepted ...");
        
        // Simply abort() will cause the fuzzer treat it as a crash, so we need to set a flag to ignore it
        let state = _state.expect("No state found");
        let exec_meta = state
            .metadata_map_mut()
            .get_mut::<ExecMeta>()
            .expect("Can't get exec_meta");
        exec_meta.ignore = true;
        
        abort();
    }
    else {
        SyscallHookResult::new(None)
    }
}
