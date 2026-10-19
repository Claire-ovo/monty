//! `console`: output through the session's `PrintWriter`, formatted as Node does.

use std::borrow::Cow;

use monty_types::PrintStream;

use super::{NativeFn, install_function, plain_object};
use crate::{
    error::RunResult,
    heap::{HeapId, PropFlags},
    value::JsValue,
    vm::{Vm, number::number_to_string},
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let console = plain_object(vm);
    for native in [
        NativeFn::ConsoleLog,
        NativeFn::ConsoleError,
        NativeFn::ConsoleWarn,
        NativeFn::ConsoleInfo,
        NativeFn::ConsoleDebug,
    ] {
        install_function(vm, console, native);
    }
    super::install_value(vm, global, "console", JsValue::Ref(console), PropFlags::HIDDEN);
}

pub(super) fn log(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    write(vm, PrintStream::Stdout, args)
}

pub(super) fn error(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    write(vm, PrintStream::Stderr, args)
}

/// Formats the arguments (`%` directives when the first is a string) and
/// writes one line.
fn write(vm: &mut Vm<'_>, stream: PrintStream, args: &[JsValue]) -> RunResult<JsValue> {
    let text = match args.first() {
        Some(first)
            if first.is_string()
                && vm
                    .string_of(first)
                    .is_some_and(|s| s.units().any(|u| u == u16::from(b'%'))) =>
        {
            format_directives(vm, args)?
        }
        _ => vm.format_log_args(args)?,
    };
    let outcome = vm
        .print
        .write(stream, Cow::Owned(text))
        .and_then(|()| vm.print.push(stream, '\n'));
    if let Err(exc) = outcome {
        return Err(Vm::print_failure(&exc));
    }
    Ok(JsValue::Undefined)
}

/// `util.format`'s `%s %d %i %f %j %o %O %c %%` directives.
fn format_directives(vm: &mut Vm<'_>, args: &[JsValue]) -> RunResult<String> {
    let template = vm.string_of(&args[0]).expect("string").to_string_lossy();
    let mut out = String::with_capacity(template.len());
    let mut next = 1;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(directive) = chars.peek().copied() else {
            out.push('%');
            break;
        };
        if directive == '%' {
            chars.next();
            out.push('%');
            continue;
        }
        if !matches!(directive, 's' | 'd' | 'i' | 'f' | 'j' | 'o' | 'O' | 'c') || next >= args.len() {
            out.push('%');
            continue;
        }
        chars.next();
        let value = &args[next];
        next += 1;
        match directive {
            's' => match value {
                JsValue::Atom(_) | JsValue::Str(_) => {
                    out.push_str(&vm.string_of(value).expect("string").to_string_lossy());
                }
                JsValue::Ref(_) => out.push_str(&vm.inspect(value)?),
                other => out.push_str(&vm.coerce_string(other)?.to_string_lossy()),
            },
            'd' | 'i' => {
                let number = if value.is_object() {
                    f64::NAN
                } else {
                    vm.coerce_number(value)?
                };
                let number = if directive == 'i' { number.trunc() } else { number };
                out.push_str(&number_to_string(number));
            }
            'f' => {
                let number = vm.coerce_number(value)?;
                out.push_str(&number_to_string(number));
            }
            'j' => {
                out.push_str(&super::json::stringify_to_string(vm, value)?.unwrap_or_else(|| "undefined".to_owned()));
            }
            'o' => out.push_str(&vm.inspect_verbose(value)?),
            'O' => out.push_str(&vm.inspect(value)?),
            _ => {} // `%c` consumes its argument and prints nothing
        }
    }
    for value in &args[next..] {
        out.push(' ');
        match value {
            JsValue::Atom(_) | JsValue::Str(_) => out.push_str(&vm.string_of(value).expect("string").to_string_lossy()),
            other => out.push_str(&vm.inspect(other)?),
        }
    }
    Ok(out)
}
