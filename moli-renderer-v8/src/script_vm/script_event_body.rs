//! Body-only dispatch for script-element terminal events and internal Window errors.
//!
//! These primitives may enter author code, but they never decide that an HTML
//! task or an algorithm step has ended. Selected Page tasks and the few
//! synchronous parser/module/runtime algorithms that still need a checkpoint
//! consume them through their own named completion boundary.

use anyhow::{Result, anyhow};

use super::ScriptVm;
use crate::context_bootstrap::{
    ORIGINAL_WEBASSEMBLY_COMPILE_ERROR_CONSTRUCTOR_SLOT,
    ORIGINAL_WEBASSEMBLY_LINK_ERROR_CONSTRUCTOR_SLOT, dispatch_window_error_event_with_details,
};
use crate::host::ScriptEventTask;
use crate::util::{get_private_value, v8_string, v8str};

impl ScriptVm {
    pub(crate) fn dispatch_script_event_body_best_effort(&mut self, task: &ScriptEventTask) {
        if let Err(error) = self.dispatch_script_event_body(task) {
            self.record_runtime_warning(format_args!(
                "script {} body dispatch failed for `{}`: {error}",
                task.event_name(),
                task.handle
            ));
        }
    }

    pub(crate) fn dispatch_script_event_body(&mut self, task: &ScriptEventTask) -> Result<()> {
        self.with_main_world_document_scope(|scope, host_ptr, document_runtime| {
            document_runtime
                .host_dispatch_script_event(scope, host_ptr, task)
                .map_err(anyhow::Error::msg)
        })
    }

    pub(crate) fn report_window_error_body_best_effort(
        &mut self,
        message: &str,
        filename: Option<&str>,
        error_constructor: Option<crate::types::ScriptErrorConstructorKind>,
    ) {
        if let Err(error) = self.report_window_error_body(message, filename, error_constructor) {
            self.record_runtime_warning(format_args!(
                "window script failure body dispatch failed for `{}`: {error}",
                filename.unwrap_or("")
            ));
        }
    }

    pub(crate) fn report_window_error_body(
        &mut self,
        message: &str,
        filename: Option<&str>,
        error_constructor: Option<crate::types::ScriptErrorConstructorKind>,
    ) -> Result<()> {
        // An evaluated module keeps the original thrown value, including a TLA
        // rejection, in its V8 module record. Read it only at the existing error
        // reporting boundary, after module evaluation has finished unwinding.
        let failed_module = filename
            .filter(|_| message.starts_with("NativeEsmEvaluateFailed:"))
            .and_then(|filename| url::Url::parse(filename).ok())
            .and_then(|url| {
                self.document_runtime
                    .native_module_entry_id(&crate::module_runtime::ModuleMapKey::java_script(url))
            })
            .and_then(|entry| self.document_runtime.native_compiled_module(entry));
        self.with_default_context_scope(|scope, host_ptr| {
            if let Some(module) = &failed_module {
                let module = v8::Local::new(scope, module);
                if module.get_status() == v8::ModuleStatus::Errored {
                    let reason = module.get_exception();
                    let mut snapshot =
                        crate::exception_reporting::runtime_exception_snapshot(scope, reason, true);
                    if snapshot.filename.is_empty() {
                        snapshot.filename = filename.unwrap_or_default().to_owned();
                    }
                    return dispatch_window_error_event_with_details(
                        scope,
                        host_ptr,
                        &snapshot.message,
                        &snapshot.filename,
                        snapshot.lineno,
                        snapshot.colno,
                        Some(reason),
                    )
                    .map_err(anyhow::Error::msg);
                }
            }
            let global = scope.get_current_context().global(scope);
            let message_value = v8_string(scope, message)
                .ok_or_else(|| anyhow!("failed to allocate reportError message"))?;
            let error_value =
                window_script_failure_error_value(scope, global, error_constructor, message_value);
            crate::exception_reporting::mark_synthetic_reporting_exception(scope, error_value);
            if let Some(filename) = filename
                && let Some(filename_value) = v8_string(scope, filename)
                && let Ok(error_object) = v8::Local::<v8::Object>::try_from(error_value)
            {
                let _ = error_object.set(
                    scope,
                    v8str(scope, "fileName").into(),
                    filename_value.into(),
                );
            }
            // Internal script failure reporting must not call the page-visible
            // window.reportError function while lifecycle state is unwinding.
            // Dispatch the equivalent ErrorEvent body directly.
            dispatch_window_error_event_with_details(
                scope,
                host_ptr,
                message,
                filename.unwrap_or(""),
                0,
                0,
                Some(error_value),
            )
            .map_err(anyhow::Error::msg)
        })
    }
}

fn window_script_failure_error_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    error_constructor: Option<crate::types::ScriptErrorConstructorKind>,
    message: v8::Local<'s, v8::String>,
) -> v8::Local<'s, v8::Value> {
    let constructor = match error_constructor {
        Some(crate::types::ScriptErrorConstructorKind::WebAssemblyCompileError) => {
            original_webassembly_error_constructor(
                scope,
                global,
                ORIGINAL_WEBASSEMBLY_COMPILE_ERROR_CONSTRUCTOR_SLOT,
            )
        }
        Some(crate::types::ScriptErrorConstructorKind::WebAssemblyLinkError) => {
            original_webassembly_error_constructor(
                scope,
                global,
                ORIGINAL_WEBASSEMBLY_LINK_ERROR_CONSTRUCTOR_SLOT,
            )
        }
        _ => None,
    };
    constructor
        .and_then(|constructor| constructor.new_instance(scope, &[message.into()]))
        .map(v8::Local::<v8::Value>::from)
        .unwrap_or_else(|| match error_constructor {
            Some(crate::types::ScriptErrorConstructorKind::SyntaxError) => {
                v8::Exception::syntax_error(scope, message)
            }
            Some(crate::types::ScriptErrorConstructorKind::TypeError) => {
                v8::Exception::type_error(scope, message)
            }
            _ => v8::Exception::error(scope, message),
        })
}

fn original_webassembly_error_constructor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    original_slot: &str,
) -> Option<v8::Local<'s, v8::Function>> {
    get_private_value(scope, global, original_slot)
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
}
