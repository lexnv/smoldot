// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Wasm virtual machine, with automatic storage overlay and logs management.

// TODO: more docs

use crate::executor::{self, host, vm};

use alloc::{string::String, vec::Vec};
use core::fmt;

/// Configuration for [`run`].
pub struct Config<'a, TParams> {
    /// Virtual machine to be run.
    pub virtual_machine: host::HostVmPrototype,

    /// Name of the function to be called.
    pub function_to_call: &'a str,

    /// Parameter of the call, as an iterator of bytes. The concatenation of bytes forms the
    /// actual input.
    pub parameter: TParams,

    /// Maximum log level of the runtime.
    ///
    /// > **Note**: This value is opaque from the point of the view of the client, and the runtime
    /// >           is free to interpret it the way it wants. However, usually values are: `0` for
    /// >           "off", `1` for "error", `2` for "warn", `3` for "info", `4` for "debug",
    /// >           and `5` for "trace".
    pub max_log_level: u32,
}

/// Start running the WebAssembly virtual machine.
pub fn run(
    config: Config<impl Iterator<Item = impl AsRef<[u8]>> + Clone>,
) -> Result<RuntimeHostVm, (host::StartErr, host::HostVmPrototype)> {
    Ok(Inner {
        vm: config
            .virtual_machine
            .run_vectored(config.function_to_call, config.parameter)?
            .into(),
        logs: String::new(),
        max_log_level: config.max_log_level,
    }
    .run())
}

/// Execution is successful.
#[derive(Debug)]
pub struct Success {
    /// Contains the output value of the runtime, and the virtual machine that was passed at
    /// initialization.
    pub virtual_machine: SuccessVirtualMachine,
    /// Concatenation of all the log messages printed by the runtime.
    pub logs: String,
}

/// Function execution has succeeded. Contains the return value of the call.
pub struct SuccessVirtualMachine(host::Finished);

impl SuccessVirtualMachine {
    /// Returns the value the called function has returned.
    pub fn value(&'_ self) -> impl AsRef<[u8]> + '_ {
        self.0.value()
    }

    /// Turns the virtual machine back into a prototype.
    pub fn into_prototype(self) -> host::HostVmPrototype {
        self.0.into_prototype()
    }
}

impl fmt::Debug for SuccessVirtualMachine {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("SuccessVirtualMachine").finish()
    }
}

/// Error that can happen during the execution.
#[derive(Debug, derive_more::Display)]
#[display(fmt = "{detail}")]
pub struct Error {
    /// Exact error that happened.
    pub detail: ErrorDetail,
    /// Prototype of the virtual machine that was passed through [`Config::virtual_machine`].
    pub prototype: host::HostVmPrototype,
}

/// See [`Error::detail`].
#[derive(Debug, Clone, derive_more::Display)]
pub enum ErrorDetail {
    /// Error while executing the Wasm virtual machine.
    #[display(fmt = "Error while executing Wasm VM: {error}\n{logs:?}")]
    WasmVm {
        /// Error that happened.
        error: host::Error,
        /// Concatenation of all the log messages printed by the runtime.
        logs: String,
    },
    /// Size of the logs generated by the runtime exceeds the limit.
    LogsTooLong,
    ForbiddenHostCall,
}

/// Current state of the execution.
#[must_use]
pub enum RuntimeHostVm {
    /// Execution is over.
    Finished(Result<Success, Error>),
    /// Loading a storage value is required in order to continue.
    StorageGet(StorageGet),
    /// Fetching the key that follows a given one is required in order to continue.
    NextKey(NextKey),
    /// Fetching the storage trie root is required in order to continue.
    StorageRoot(StorageRoot),
    /// Verifying whether a signature is correct is required in order to continue.
    SignatureVerification(SignatureVerification),
}

impl RuntimeHostVm {
    /// Cancels execution of the virtual machine and returns back the prototype.
    pub fn into_prototype(self) -> host::HostVmPrototype {
        match self {
            RuntimeHostVm::Finished(Ok(inner)) => inner.virtual_machine.into_prototype(),
            RuntimeHostVm::Finished(Err(inner)) => inner.prototype,
            RuntimeHostVm::StorageGet(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::NextKey(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::StorageRoot(inner) => inner.inner.vm.into_prototype(),
            RuntimeHostVm::SignatureVerification(inner) => inner.inner.vm.into_prototype(),
        }
    }
}

/// Loading a storage value is required in order to continue.
#[must_use]
pub struct StorageGet {
    inner: Inner,
}

impl StorageGet {
    /// Returns the key whose value must be passed to [`StorageGet::inject_value`].
    pub fn key(&'_ self) -> impl AsRef<[u8]> + '_ {
        match &self.inner.vm {
            host::HostVm::ExternalStorageGet(req) => match req.key() {
                // TODO: child tries are not implemented correctly
                host::StorageKey::MainTrie { key } => key,
                _ => unreachable!(),
            },

            // We only create a `StorageGet` if the state is one of the above.
            _ => unreachable!(),
        }
    }

    /// Injects the corresponding storage value.
    pub fn inject_value(
        mut self,
        value: Option<impl Iterator<Item = impl AsRef<[u8]>>>,
    ) -> RuntimeHostVm {
        // TODO: update the implementation to not require the folding here
        let value = value.map(|i| {
            i.fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            })
        });

        match self.inner.vm {
            host::HostVm::ExternalStorageGet(req) => {
                // TODO: should actually report the offset and max_size in the API
                self.inner.vm = req.resume_full_value(value.as_ref().map(|v| &v[..]));
            }

            // We only create a `StorageGet` if the state is one of the above.
            _ => unreachable!(),
        };

        self.inner.run()
    }
}

/// Fetching the key that follows a given one is required in order to continue.
#[must_use]
pub struct NextKey {
    inner: Inner,
}

impl NextKey {
    /// Returns the key whose next key must be passed back.
    pub fn key(&'_ self) -> impl AsRef<[u8]> + '_ {
        match &self.inner.vm {
            host::HostVm::ExternalStorageNextKey(req) => match req.key() {
                // TODO: child tries are not implemented correctly
                host::StorageKey::MainTrie { key } => key,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    /// If `true`, then the provided value must the one superior or equal to the requested key.
    /// If `false`, then the provided value must be strictly superior to the requested key.
    pub fn or_equal(&self) -> bool {
        false
    }

    /// If `true`, then the search must include both branch nodes and storage nodes. If `false`,
    /// the search only covers storage nodes.
    pub fn branch_nodes(&self) -> bool {
        false
    }

    /// Returns the prefix the next key must start with. If the next key doesn't start with the
    /// given prefix, then `None` should be provided.
    pub fn prefix(&'_ self) -> impl AsRef<[u8]> + '_ {
        &[][..]
    }

    /// Injects the key.
    ///
    /// # Panic
    ///
    /// Panics if the key passed as parameter isn't strictly superior to the requested key.
    ///
    pub fn inject_key(mut self, key: Option<impl AsRef<[u8]>>) -> RuntimeHostVm {
        let key = key.as_ref().map(|k| k.as_ref());

        match self.inner.vm {
            host::HostVm::ExternalStorageNextKey(req) => {
                self.inner.vm = req.resume(key.as_ref().map(|v| &v[..]));
            }

            // We only create a `NextKey` if the state is the one above.
            _ => unreachable!(),
        };

        self.inner.run()
    }
}

/// Fetching the storage trie root is required in order to continue.
#[must_use]
pub struct StorageRoot {
    inner: Inner,
}

impl StorageRoot {
    /// Writes the trie root hash to the Wasm VM and prepares it for resume.
    pub fn resume(mut self, hash: &[u8; 32]) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::ExternalStorageRoot(req) => {
                self.inner.vm = req.resume(Some(hash));
            }

            // We only create a `StorageRoot` if the state is the one above.
            _ => unreachable!(),
        };

        self.inner.run()
    }
}

/// Verifying whether a signature is correct is required in order to continue.
#[must_use]
pub struct SignatureVerification {
    inner: Inner,
}

impl SignatureVerification {
    /// Returns the message that the signature is expected to sign.
    pub fn message(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.message(),
            _ => unreachable!(),
        }
    }

    /// Returns the signature.
    ///
    /// > **Note**: Be aware that this signature is untrusted input and might not be part of the
    /// >           set of valid signatures.
    pub fn signature(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.signature(),
            _ => unreachable!(),
        }
    }

    /// Returns the public key the signature is against.
    ///
    /// > **Note**: Be aware that this public key is untrusted input and might not be part of the
    /// >           set of valid public keys.
    pub fn public_key(&'_ self) -> impl AsRef<[u8]> + '_ {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.public_key(),
            _ => unreachable!(),
        }
    }

    /// Verify the signature. Returns `true` if it is valid.
    pub fn is_valid(&self) -> bool {
        match self.inner.vm {
            host::HostVm::SignatureVerification(ref sig) => sig.is_valid(),
            _ => unreachable!(),
        }
    }

    /// Verify the signature and resume execution.
    pub fn verify_and_resume(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.verify_and_resume(),
            _ => unreachable!(),
        }

        self.inner.run()
    }

    /// Resume the execution assuming that the signature is valid.
    ///
    /// > **Note**: You are strongly encouraged to call
    /// >           [`SignatureVerification::verify_and_resume`]. This function is meant to be
    /// >           used only in debugging situations.
    pub fn resume_success(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.resume_success(),
            _ => unreachable!(),
        }

        self.inner.run()
    }

    /// Resume the execution assuming that the signature is invalid.
    ///
    /// > **Note**: You are strongly encouraged to call
    /// >           [`SignatureVerification::verify_and_resume`]. This function is meant to be
    /// >           used only in debugging situations.
    pub fn resume_failed(mut self) -> RuntimeHostVm {
        match self.inner.vm {
            host::HostVm::SignatureVerification(sig) => self.inner.vm = sig.resume_failed(),
            _ => unreachable!(),
        }

        self.inner.run()
    }
}

/// Implementation detail of the execution. Shared by all the variants of [`RuntimeHostVm`]
/// other than [`RuntimeHostVm::Finished`].
struct Inner {
    /// Virtual machine running the call.
    vm: host::HostVm,
    /// Concatenation of all the log messages generated by the runtime.
    logs: String,
    /// Value provided by [`Config::max_log_level`].
    max_log_level: u32,
}

impl Inner {
    /// Continues the execution.
    fn run(mut self) -> RuntimeHostVm {
        loop {
            match self.vm {
                host::HostVm::ReadyToRun(r) => self.vm = r.run(),

                host::HostVm::Error { error, prototype } => {
                    return RuntimeHostVm::Finished(Err(Error {
                        detail: ErrorDetail::WasmVm {
                            error,
                            logs: self.logs,
                        },
                        prototype,
                    }));
                }

                host::HostVm::Finished(finished) => {
                    return RuntimeHostVm::Finished(Ok(Success {
                        virtual_machine: SuccessVirtualMachine(finished),
                        logs: self.logs,
                    }));
                }

                host::HostVm::ExternalStorageGet(req) => {
                    let is_main_trie = matches!(req.key(), host::StorageKey::MainTrie { .. });
                    if is_main_trie {
                        self.vm = req.into();
                        return RuntimeHostVm::StorageGet(StorageGet { inner: self });
                    } else {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None)
                    }
                }

                host::HostVm::ExternalStorageNextKey(req) => {
                    let is_main_trie = matches!(req.key(), host::StorageKey::MainTrie { .. });
                    if is_main_trie {
                        self.vm = req.into();
                        return RuntimeHostVm::NextKey(NextKey { inner: self });
                    } else {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None)
                    }
                }

                host::HostVm::ExternalStorageNextChildTrie(req) => {
                    // TODO: this is a dummy implementation and child tries are not implemented properly
                    self.vm = req.resume(None);
                }

                host::HostVm::SignatureVerification(req) => {
                    self.vm = req.into();
                    return RuntimeHostVm::SignatureVerification(SignatureVerification {
                        inner: self,
                    });
                }

                host::HostVm::CallRuntimeVersion(req) => {
                    // TODO: make the user execute this ; see https://github.com/paritytech/smoldot/issues/144
                    // The code below compiles the provided WebAssembly runtime code, which is a
                    // relatively expensive operation (in the order of milliseconds).
                    // While it could be tempting to use a system cache, this function is expected
                    // to be called only right before runtime upgrades. Considering that runtime
                    // upgrades are quite uncommon and that a caching system is rather non-trivial
                    // to set up, the approach of recompiling every single time is preferred here.
                    // TODO: number of heap pages?! we use the default here, but not sure whether that's correct or if we have to take the current heap pages
                    let vm_prototype = match host::HostVmPrototype::new(host::Config {
                        module: req.wasm_code(),
                        heap_pages: executor::DEFAULT_HEAP_PAGES,
                        exec_hint: vm::ExecHint::Oneshot,
                        allow_unresolved_imports: false, // TODO: what is a correct value here?
                    }) {
                        Ok(w) => w,
                        Err(_) => {
                            self.vm = req.resume(Err(()));
                            continue;
                        }
                    };

                    self.vm = req.resume(Ok(vm_prototype.runtime_version().as_ref()));
                }

                host::HostVm::ExternalStorageRoot(req) => {
                    let is_main_trie = matches!(req.trie(), host::Trie::MainTrie);
                    if is_main_trie {
                        self.vm = req.into();
                        return RuntimeHostVm::StorageRoot(StorageRoot { inner: self });
                    } else {
                        // TODO: this is a dummy implementation and child tries are not implemented properly
                        self.vm = req.resume(None)
                    }
                }

                host::HostVm::GetMaxLogLevel(resume) => {
                    self.vm = resume.resume(self.max_log_level);
                }

                host::HostVm::LogEmit(req) => {
                    // We add a hardcoded limit to the logs generated by the runtime in order to
                    // make sure that there is no memory leak. In practice, the runtime should
                    // rarely log more than a few hundred bytes. This limit is hardcoded rather
                    // than configurable because it is not expected to be reachable unless
                    // something is very wrong.
                    struct WriterWithMax<'a>(&'a mut String);
                    impl<'a> fmt::Write for WriterWithMax<'a> {
                        fn write_str(&mut self, s: &str) -> fmt::Result {
                            if self.0.len().saturating_add(s.len()) >= 1024 * 1024 {
                                return Err(fmt::Error);
                            }
                            self.0.push_str(s);
                            Ok(())
                        }
                        fn write_char(&mut self, c: char) -> fmt::Result {
                            if self.0.len().saturating_add(1) >= 1024 * 1024 {
                                return Err(fmt::Error);
                            }
                            self.0.push(c);
                            Ok(())
                        }
                    }
                    match fmt::write(&mut WriterWithMax(&mut self.logs), format_args!("{req}")) {
                        Ok(()) => {}
                        Err(fmt::Error) => {
                            return RuntimeHostVm::Finished(Err(Error {
                                detail: ErrorDetail::LogsTooLong,
                                prototype: host::HostVm::LogEmit(req).into_prototype(),
                            }));
                        }
                    }
                    self.vm = req.resume();
                }

                other => {
                    return RuntimeHostVm::Finished(Err(Error {
                        detail: ErrorDetail::ForbiddenHostCall,
                        prototype: other.into_prototype(),
                    }))
                }
            }
        }
    }
}
