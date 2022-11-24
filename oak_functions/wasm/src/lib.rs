//
// Copyright 2022 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! Wasm business logic provider based on [Wasmi](https://github.com/paritytech/wasmi).

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use anyhow::Context;
use byteorder::{ByteOrder, LittleEndian};
use hashbrown::HashMap;
use oak_functions_abi::{
    proto::{ExtensionHandle, OakStatus},
    Request, Response, StatusCode,
};
use oak_functions_extension::{ExtensionFactory, OakApiNativeExtension};
use oak_logger::{Level, OakLogger};
use wasmi::{core::ValueType, AsContext, AsContextMut, Func, MemoryType, Store};

const MAIN_FUNCTION_NAME: &str = "main";
const ALLOC_FUNCTION_NAME: &str = "alloc";

// Type aliases for positions and offsets in Wasm linear memory. Any future 64-bit version
// of Wasm would use different types.
pub type AbiPointer = u32;
pub type AbiPointerOffset = u32;
// Type alias for the ExtensionHandle type, which has to be cast into a ExtensionHandle.
pub type AbiExtensionHandle = i32;
/// Wasm type identifier for position/offset values in linear memory. Any future 64-bit version of
/// Wasm would use a different value.
pub const ABI_USIZE: ValueType = ValueType::I32;

// TODO(mschett): Check whether this needs to be public.
pub struct UserState {
    request_bytes: Vec<u8>,
    response_bytes: Vec<u8>,
    extensions: HashMap<ExtensionHandle, Box<dyn OakApiNativeExtension>>,
}

impl UserState {
    // We initialize user state with the empty response.
    fn init(
        request_bytes: Vec<u8>,
        extensions: HashMap<ExtensionHandle, Box<dyn OakApiNativeExtension>>,
    ) -> Self {
        UserState {
            request_bytes,
            response_bytes: Vec::new(),
            extensions,
        }
    }

    pub fn get_extension(
        &mut self,
        handle: i32,
    ) -> Result<&mut Box<dyn OakApiNativeExtension>, OakStatus> {
        let handle: ExtensionHandle = ExtensionHandle::from_i32(handle).ok_or_else(|| {
            // TODO(mschett): Fix logging.
            // self.log_error(&format!("Fail to convert handle {:?} from i32.", handle));
            OakStatus::ErrInvalidHandle
        })?;

        let extension = match self.extensions.get_mut(&handle) {
            // Can't convince the borrow checker to use `ok_or_else` to `self.log_error`.
            Some(extension) => Ok(extension),
            None => {
                // TODO(mschett): Fix logging.
                // self.log_error(&format!("Cannot find extension with handle {:?}.", handle));
                Err(OakStatus::ErrInvalidHandle)
            }
        };

        extension
    }
}

impl alloc::fmt::Debug for UserState {
    fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        formatter
            .debug_struct("UserState")
            .field("request_bytes", &self.request_bytes)
            .field("response_bytes", &self.response_bytes)
            .field("extensions", &self.extensions)
            .finish()
    }
}

/// `WasmState` holds runtime values for a particular execution instance of Wasm, handling a
/// single user request. The methods here correspond to the ABI host functions that allow the Wasm
/// module to exchange the request and the response with the Oak functions server. These functions
/// translate values between Wasm linear memory and Rust types.
pub struct WasmState<L: OakLogger> {
    instance: wasmi::Instance,
    store: wasmi::Store<UserState>,
    logger: L,
}

impl<L> WasmState<L>
where
    L: OakLogger,
{
    pub fn new(
        wasm_module_bytes: Vec<u8>,
        request_bytes: Vec<u8>,
        logger: L,
        extensions: HashMap<ExtensionHandle, Box<dyn OakApiNativeExtension>>,
    ) -> anyhow::Result<Self> {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, &wasm_module_bytes[..])
            .map_err(|err| anyhow::anyhow!("couldn't load module from buffer: {:?}", err))?;

        let user_state = UserState::init(request_bytes, extensions);

        // For isolated requests we need to create a new store for every request.
        let mut store = wasmi::Store::new(&module.engine(), user_state);

        let mut linker: wasmi::Linker<UserState> = wasmi::Linker::new();

        // TODO(mschett): Check what we need this variable for.
        let host = "oak_functions";

        // TODO(mschett): It would be nice to define the linker in WasmHandler to reuse it for
        // different WasmStates, but the externals Func and Memory defined in the linker depend on
        // store, which needs to be new for every WasmState. In contrast, in wasmtime `func_wrap`
        // does not depend on store (https://docs.rs/wasmtime/latest/wasmtime/struct.Linker.html#method.func_wrap).

        // Add memory to linker.
        // TODO(mschett): Check what is a sensible initial value.
        let initial_memory_size = 10;
        // TODO(mschett): Fix unwrap.
        let memory_type = MemoryType::new(initial_memory_size, None).unwrap();
        let memory = wasmi::Memory::new(&mut store, memory_type).unwrap();
        // TODO(mschett): Fix to .context("Failed to initialize Wasm memory.");
        linker
            .define(host, "memory", wasmi::Extern::Memory(memory))
            .unwrap();

        let read_request = wasmi::Func::wrap(
            &mut store,
            // TODO(mschett): Check types of params with oak_functions_resolve_funcs.
            |mut caller: wasmi::Caller<'_, UserState>, buf_ptr_ptr: u32, buf_len_ptr: u32| {
                let oak_status = read_request(&mut caller, buf_ptr_ptr, buf_len_ptr);
                from_oak_status_result(oak_status)
            },
        );

        // TODO(mschett): Handle error.
        linker.define(host, "read_request", read_request).unwrap();

        let write_response = wasmi::Func::wrap(
            &mut store,
            // TODO(mschett): Check types of params with oak_functions_resolve_funcs.
            |mut caller: wasmi::Caller<'_, UserState>, buf_ptr: u32, buf_len: u32| {
                let result = write_response(&mut caller, buf_ptr, buf_len);
                from_oak_status_result(result)
            },
        );

        // TODO(mschett): Handle error.
        linker
            .define(host, "write_response", write_response)
            .unwrap();

        let invoke_extension = wasmi::Func::wrap(
            &mut store,
            // TODO(mschett): Check types of params with oak_functions_resolve_funcs.
            |mut caller: wasmi::Caller<'_, UserState>,
             handle: i32,
             request_ptr: u32,
             request_len: u32,
             response_ptr_ptr: u32,
             response_len_ptr: u32| {
                let result = invoke_extension(
                    &mut caller,
                    handle,
                    request_ptr,
                    request_len,
                    response_ptr_ptr,
                    response_len_ptr,
                );

                from_oak_status_result(result)
            },
        );

        // TODO(mschett): Handle error.
        linker.define(host, "invoke", invoke_extension).unwrap();

        // Use linker and store to get instance of module.
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|err| anyhow::anyhow!("failed to instantiate Wasm module: {:?}", err))?
            .ensure_no_start(&mut store)
            .map_err(|err| {
                anyhow::anyhow!("failed to ensure no start in Wasm module: {:?}", err)
            })?;
        // TODO(mschett): Check why we do ensure no start.

        // Check that the instance exports "main".
        check_export_func_type(
            &instance,
            &store,
            MAIN_FUNCTION_NAME,
            // TODO(mschett): Check whether I give the right func_type for main.
            wasmi::FuncType::new([], []),
        )
        .context("couldn't validate `main` export")?;

        // Check that the instance exports "alloc".
        check_export_func_type(
            &instance,
            &store,
            ALLOC_FUNCTION_NAME,
            // TODO(mschett): Check whether I give the right func_type for alloc.
            wasmi::FuncType::new([ValueType::I32], [ValueType::I32]),
        )
        .context("couldn't validate `alloc` export")?;

        // Make sure that non-empty `memory` is attached to the instance. Fail early if
        // `memory` is not available.
        instance
            .get_export(&store, "memory")
            .ok_or(anyhow::anyhow!("couldn't find Wasm `memory` export"))?
            .into_memory()
            .ok_or(anyhow::anyhow!(
                "couldn't interpret Wasm `memory` export as memory"
            ))?;

        let wasm_state = Self {
            instance,
            store,
            logger,
        };

        Ok(wasm_state)
    }

    fn invoke(&mut self) {
        // TODO(mschett): Fix unwrap.
        let main = self
            .instance
            .get_export(&self.store, MAIN_FUNCTION_NAME)
            .unwrap()
            .into_func()
            .unwrap();
        let result = main.call(&mut self.store, &[], &mut []);
        self.logger.log_sensitive(
            Level::Info,
            &format!("running Wasm module completed with result: {:?}", result),
        );
    }

    fn get_request_bytes(&self) -> Vec<u8> {
        let user_state = self.store.state();
        user_state.request_bytes.clone()
    }

    fn get_response_bytes(&self) -> Vec<u8> {
        let user_state = self.store.state();
        user_state.response_bytes.clone()
    }

    /// Validates whether a given address range (inclusive) falls within the currently allocated
    /// range of guest memory.
    /* TODO(mschett): Check if we still need this.

    fn validate_range(&self, addr: AbiPointer, offset: AbiPointerOffset) -> Result<(), OakStatus> {
        let memory = self
            .instance
            .get_export(&self.store, "memory")
            // TODO(mschett): Fix unwrap.
            .unwrap()
            .into_memory()
            .expect("WasmState memory not attached!?");

        // TODO(mschett): Check if there is a better way to check the memory size.
        let memory_size: wasmi::core::memory_units::Bytes =
            wasmi::core::memory_units::Pages::from(memory.current_pages(&self.store)).into();
        // Check whether the end address is below or equal to the size of the guest memory.
        if wasmi::core::memory_units::Bytes((addr as usize) + (offset as usize)) <= memory_size {
            Ok(())
        } else {
            Err(OakStatus::ErrInvalidArgs)
        }
    }
     */

    fn log_error(&self, message: &str) {
        self.logger.log_sensitive(Level::Error, message)
    }
}

// Calls given alloc Func with ctx and length as parameters.
// alloc Func has to belong to given ctx.
pub fn call_alloc(ctx: &mut impl AsContextMut, alloc: Func, len: i32) -> AbiPointer {
    let inputs = &[wasmi::core::Value::I32(len)];
    // TODO(mschett): Check whether putting default value 0 is a good idea.
    let mut outputs = [wasmi::core::Value::I32(0); 1];

    alloc
        .call(ctx, inputs, &mut outputs)
        .expect("`alloc` call failed");

    let result_value = outputs;

    match result_value[0] {
        wasmi::core::Value::I32(v) => v as u32,
        _ => panic!("invalid value type returned from `alloc`"),
    }
}

/// Reads the buffer starting at address `buf_ptr` with length `buf_len` from the Wasm memory.
pub fn read_buffer(
    ctx: &mut impl AsContext,
    memory: &mut wasmi::Memory,
    buf_ptr: AbiPointer,
    buf_len: AbiPointerOffset,
) -> Result<Vec<u8>, OakStatus> {
    let mut target = alloc::vec![0; buf_len as usize];
    // TODO(mschett): check usize cast.
    memory
        .read(&ctx, buf_ptr as usize, &mut target)
        .map_err(|_err| {
            // TODO(mschett): Add logging.
            /*
            self.logger.log_sensitive(
                Level::Error,
                &format!("Unable to read buffer from guest memory: {:?}", err),
            ); */
            OakStatus::ErrInvalidArgs
        })?;
    Ok(target)
}

//// Read the u32 value at the `address` from the Wasm memory.
pub fn read_u32(
    ctx: &mut impl AsContext,
    memory: &mut wasmi::Memory,
    address: AbiPointer,
) -> Result<u32, OakStatus> {
    let address = read_buffer(ctx, memory, address, 4).map_err(|_err| {
        //TODO(mschett): Fix logging
        /*
        self.logger.log_sensitive(
            Level::Error,
            &format!("Unable to read u32 value from guest memory: {:?}", err),
        );
         */
        OakStatus::ErrInvalidArgs
    })?;
    let address = LittleEndian::read_u32(&address);
    Ok(address)
}

///  Writes the u32 `value` at the `address` of the Wasm memory.
pub fn write_u32(
    ctx: &mut impl AsContextMut,
    memory: &mut wasmi::Memory,
    value: u32,
    address: AbiPointer,
) -> Result<(), OakStatus> {
    let value_bytes = &mut [0; 4];
    LittleEndian::write_u32(value_bytes, value);
    write_buffer(ctx, memory, value_bytes, address).map_err(|_err| {
        // TODO(mschett): Fix logging
        /*
        self.logger.log_sensitive(
            Level::Error,
            &format!("Unable to write u32 value into guest memory: {:?}", err),
        );
        */
        OakStatus::ErrInvalidArgs
    })
}

/// Writes the buffer `source` at the address `dest` of the Wasm memory, if `source` fits in the
/// allocated memory.
pub fn write_buffer(
    ctx: &mut impl AsContextMut,
    memory: &mut wasmi::Memory,
    source: &[u8],
    dest: AbiPointer,
) -> Result<(), OakStatus> {
    // TODO(mschett): Check whether we want to validate range.
    // self.validate_range(dest, source.len() as u32)?;
    // TODO(mschett): check usize cast.
    memory.write(ctx, dest as usize, source).map_err(|_err| {
        // TODO(mschett): Add logging.
        /*
        self.logger.log_sensitive(
            Level::Error,
            &format!("Unable to write buffer into guest memory: {:?}", err),
        );  */
        OakStatus::ErrInvalidArgs
    })
}

/// Writes the given `buffer` by allocating `buffer.len()` Wasm memory and writing the address
/// of the allocated memory to `dest_ptr_ptr` and the length to `dest_len_ptr`.
pub fn alloc_and_write_buffer(
    ctx: &mut impl AsContextMut,
    memory: &mut wasmi::Memory,
    alloc: Func,
    buffer: Vec<u8>,
    dest_ptr_ptr: AbiPointer,
    dest_len_ptr: AbiPointer,
) -> Result<(), OakStatus> {
    let dest_ptr = call_alloc(ctx, alloc, buffer.len() as i32);
    write_buffer(ctx, memory, &buffer, dest_ptr)?;
    write_u32(ctx, memory, dest_ptr, dest_ptr_ptr)?;
    write_u32(ctx, memory, buffer.len() as u32, dest_len_ptr)?;
    Ok(())
}

/// Corresponds to the host ABI function [`read_request`](https://github.com/project-oak/oak/blob/main/docs/oak_functions_abi.md#read_request).
pub fn read_request(
    caller: &mut wasmi::Caller<'_, UserState>,
    dest_ptr_ptr: AbiPointer,
    dest_len_ptr: AbiPointer,
) -> Result<(), OakStatus> {
    // TODO(mschett): Fix unwraps.
    let alloc = caller.get_export("alloc").unwrap().into_func().unwrap();

    let mut memory = caller
        .get_export("memory")
        // TODO(mschett): Fix unwrap.
        .unwrap()
        .into_memory()
        .expect("WasmState memory not attached!?");

    let request_bytes = caller.host_data().request_bytes.clone();
    let dest_ptr = call_alloc(caller, alloc, request_bytes.len() as i32);
    write_buffer(caller, &mut memory, &request_bytes, dest_ptr)?;
    write_u32(caller, &mut memory, dest_ptr, dest_ptr_ptr)?;
    write_u32(
        caller,
        &mut memory,
        request_bytes.len() as u32,
        dest_len_ptr,
    )?;
    Ok(())
}

/// Corresponds to the host ABI function [`write_response`](https://github.com/project-oak/oak/blob/main/docs/oak_functions_abi.md#write_response).
pub fn write_response(
    caller: &mut wasmi::Caller<'_, UserState>,
    buf_ptr: AbiPointer,
    buf_len: AbiPointerOffset,
) -> Result<(), OakStatus> {
    // TODO(mschett): Check what is the difference to read_buffer_from_wasm_memory.
    let memory = caller
        .get_export("memory")
        // TODO(mschett): Fix unwrap.
        .unwrap()
        .into_memory()
        .expect("WasmState memory not attached!?");

    let mut target = alloc::vec![0; buf_len as usize];

    // TODO(mschett): check usize cast.
    memory
        .read(&caller, buf_ptr as usize, &mut target)
        .map_err(|_err| {
            // TODO(mschett): Add logging.
            /*
            self.logger.log_sensitive(
                Level::Error,
                &format!(
                    "write_response(): Unable to read name from guest memory: {:?}",
                    err
                ),
            ); */
            OakStatus::ErrInvalidArgs
        })?;

    caller.host_data_mut().response_bytes = target;
    Ok(())
}

pub fn invoke_extension(
    caller: &mut wasmi::Caller<'_, UserState>,
    handle: i32,
    request_ptr: AbiPointer,
    request_len: AbiPointerOffset,
    response_ptr_ptr: AbiPointer,
    response_len_ptr: AbiPointer,
) -> Result<(), OakStatus> {
    // TODO(mschett): Fix unwraps.
    let alloc = caller.get_export("alloc").unwrap().into_func().unwrap();

    let mut memory = caller
        .get_export("memory")
        // TODO(mschett): Fix unwrap.
        .unwrap()
        .into_memory()
        .expect("WasmState memory not attached!?");

    let request = read_buffer(caller, &mut memory, request_ptr, request_len).map_err(|_err| {
        // TODO(mschett): Fix logging.
        /*
        self.log_error(&format!(
            "Handle {:?}: Unable to read input from guest memory: {:?}",
            handle, err
        )); */
        OakStatus::ErrInvalidArgs
    })?;

    let user_state = caller.host_data_mut();
    let extension = user_state.get_extension(handle)?;
    let response = extension.invoke(request)?;

    alloc_and_write_buffer(
        caller,
        &mut memory,
        alloc,
        response,
        response_ptr_ptr,
        response_len_ptr,
    )
}

// TODO(mschett): Use information from invoke_index for exported functions.
/// Invocation of a host function specified by its registered index. Acts as a wrapper for
/// the relevant native function, just:
/// - checking argument types (which should be correct as `wasmi` will only pass through those types
///   that were specified when the host function was registered with `resolve_func`).
/// - mapping resulting return/error values.
/*
fn invoke_index(
    wasm_state: WasmState,
    index: usize,
    args: wasmi::WasmParams,
) -> Result<Option<wasmi::core::Value>, wasmi::core::Trap> {
    match index {
        READ_REQUEST => from_oak_status_result(
            wasm_state.read_request(args.nth_checked(0)?, args.nth_checked(1)?),
        ),
        WRITE_RESPONSE => from_oak_status_result(
            wasm_state.write_response(args.nth_checked(0)?, args.nth_checked(1)?),
        ),
        INVOKE => from_oak_status_result(wasm_state.invoke_extension(
            args.nth_checked(0)?,
            args.nth_checked(1)?,
            args.nth_checked(2)?,
            args.nth_checked(3)?,
            args.nth_checked(4)?,
        )),
        _ => {
            // Here https://paritytech.github.io/wasmi/wasmi/trait.Externals.html#examples panics with
            //  panic!("Unimplemented function at {}.", index)
            // We prefer not to panic, and trap in an unreachable state instead.
            Err(wasmi::core::Trap::from(wasmi::core::TrapCode::Unreachable))
        }
    }
}
*/

// TODO(mschett): Use information from resolve_func for exported functions.
/*
fn resolve_func(
    wasm_state: WasmState,
    field_name: &str,
    signature: &wasmi::FuncType,
) -> Result<wasmi::Func, wasmi::Error> {
    // Look for the function (i.e., `field_name`) in the statically registered functions.
    let (index, expected_signature) = oak_functions_resolve_func(field_name)
        .ok_or_else(|| wasmi::Error::Instantiation(format!("Export {} not found", field_name)))?;

    if signature == &expected_signature {
        Ok(wasmi::Func::alloc_host(expected_signature, index))
    } else {
        Err(wasmi::Error::Instantiation(format!(
            "Export `{}` doesn't match expected signature; got: {:?}, expected: {:?}",
            field_name, signature, expected_signature
        )))
    }
}
 */

// Checks that instance exports the given export name and the func type matches the expected func
// type.
// TODO(mschett): Check if that can be shorter.
fn check_export_func_type(
    instance: &wasmi::Instance,
    store: &Store<UserState>,
    export_name: &str,
    expected_func_type: wasmi::FuncType,
) -> anyhow::Result<()> {
    let export_func_type = instance
        .get_export(store, export_name)
        .context("couldn't find Wasm export")?
        .into_func()
        .context("couldn't interpret Wasm export as function")?
        .func_type(store);
    if export_func_type != expected_func_type {
        anyhow::bail!(
            "invalid signature for export: {:?}, expected: {:?}",
            export_func_type,
            expected_func_type
        );
    } else {
        Ok(())
    }
}

// An ephemeral request handler with a Wasm module for handling the requests.
#[derive(Clone)]
pub struct WasmHandler<L: OakLogger> {
    // TODO(mschett): Check how we can avoid copying wasm_module_bytes.
    // We cannot move wasmi::Module any more, it does not implement Send.
    wasm_module_bytes: Arc<Vec<u8>>,
    extension_factories: Arc<Vec<Box<dyn ExtensionFactory<L>>>>,
    logger: L,
}

// TODO(mschett): Check whether we need the WasmHandler.
impl<L> WasmHandler<L>
where
    L: OakLogger,
{
    pub fn create(
        wasm_module_bytes: &[u8],
        extension_factories: Vec<Box<dyn ExtensionFactory<L>>>,
        logger: L,
    ) -> anyhow::Result<Self> {
        Ok(WasmHandler {
            wasm_module_bytes: Arc::new(wasm_module_bytes.to_vec()),
            extension_factories: Arc::new(extension_factories),
            logger,
        })
    }

    fn init_wasm_state(&self, request_bytes: Vec<u8>) -> anyhow::Result<WasmState<L>> {
        let mut extensions = HashMap::new();

        // Create an extension from every factory.
        for factory in self.extension_factories.iter() {
            let extension = factory.create()?;
            extensions.insert(extension.get_handle(), extension);
        }

        let wasm_state = WasmState::new(
            self.wasm_module_bytes.to_vec(),
            request_bytes,
            self.logger.clone(),
            extensions,
        )?;

        Ok(wasm_state)
    }

    pub fn handle_invoke(&self, request: Request) -> anyhow::Result<Response> {
        let response_bytes = self.handle_raw_invoke(request.body)?;
        Ok(Response::create(StatusCode::Success, response_bytes))
    }

    /// Handles an invocation using raw bytes and returns the response as raw bytes.
    pub fn handle_raw_invoke(&self, request_bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        let mut wasm_state = self.init_wasm_state(request_bytes)?;

        wasm_state.invoke();

        wasm_state
            .store
            .state_mut()
            .extensions
            .values_mut()
            .try_for_each(|e| e.terminate())?;

        Ok(wasm_state.get_response_bytes())
    }
}

/// A helper function to move between our specific result type `Result<(), OakStatus>` and the
/// `wasmi` specific result type `Result<i32, wasmi::Trap>`.
// TODO(mschett): Changed result time from Option<i32> to i32. Check implications.
fn from_oak_status_result(result: Result<(), OakStatus>) -> Result<i32, wasmi::core::Trap> {
    let oak_status: OakStatus = match result {
        Ok(()) => OakStatus::Ok,
        Err(oak_status) => oak_status,
    };
    Ok(oak_status as i32)
}
