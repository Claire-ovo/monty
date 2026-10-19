//! `Array`, `Array.prototype` and the array iterator.

use std::{iter, mem};

use super::{
    NativeFn, arg, install_constructor, install_function, install_symbol_function, integer_arg, raw_object,
    relative_index, require_callable,
};
use crate::{
    error::RunResult,
    heap::{HeapId, IterKind, ObjectClass, PropFlags, PropertyKey},
    intern::WellKnownSymbol,
    string::JsString,
    value::JsValue,
    vm::{
        Vm,
        number::{number_to_string, to_usize},
        property::IteratorStep,
    },
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let prototype = raw_object(vm, Some(object_prototype));
    vm.object_mut(prototype).expect("object").class = ObjectClass::Array;
    vm.session.realm.array_prototype = prototype;
    for native in [
        NativeFn::ArrayProtoPush,
        NativeFn::ArrayProtoPop,
        NativeFn::ArrayProtoShift,
        NativeFn::ArrayProtoUnshift,
        NativeFn::ArrayProtoSlice,
        NativeFn::ArrayProtoSplice,
        NativeFn::ArrayProtoIndexOf,
        NativeFn::ArrayProtoLastIndexOf,
        NativeFn::ArrayProtoIncludes,
        NativeFn::ArrayProtoJoin,
        NativeFn::ArrayProtoMap,
        NativeFn::ArrayProtoFilter,
        NativeFn::ArrayProtoForEach,
        NativeFn::ArrayProtoReduce,
        NativeFn::ArrayProtoReduceRight,
        NativeFn::ArrayProtoFind,
        NativeFn::ArrayProtoFindIndex,
        NativeFn::ArrayProtoFindLast,
        NativeFn::ArrayProtoFindLastIndex,
        NativeFn::ArrayProtoSome,
        NativeFn::ArrayProtoEvery,
        NativeFn::ArrayProtoConcat,
        NativeFn::ArrayProtoReverse,
        NativeFn::ArrayProtoSort,
        NativeFn::ArrayProtoFlat,
        NativeFn::ArrayProtoFlatMap,
        NativeFn::ArrayProtoFill,
        NativeFn::ArrayProtoAt,
        NativeFn::ArrayProtoCopyWithin,
        NativeFn::ArrayProtoToSorted,
        NativeFn::ArrayProtoToReversed,
        NativeFn::ArrayProtoToSpliced,
        NativeFn::ArrayProtoWith,
        NativeFn::ArrayProtoKeys,
        NativeFn::ArrayProtoEntries,
        NativeFn::ArrayProtoToString,
    ] {
        install_function(vm, prototype, native);
    }
    let values = install_function(vm, prototype, NativeFn::ArrayProtoValues);
    vm.session.realm.array_values = values;
    vm.session.heap.inc_ref(values);
    vm.define_with_flags(
        prototype,
        PropertyKey::Symbol(WellKnownSymbol::Iterator.id()),
        JsValue::Ref(values),
        PropFlags::HIDDEN,
    );
    let constructor = install_constructor(vm, global, NativeFn::Array, prototype);
    for native in [NativeFn::ArrayIsArray, NativeFn::ArrayFrom, NativeFn::ArrayOf] {
        install_function(vm, constructor, native);
    }
    // %IteratorPrototype% and %ArrayIteratorPrototype%
    let iterator_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.iterator_prototype = iterator_prototype;
    install_symbol_function(
        vm,
        iterator_prototype,
        WellKnownSymbol::Iterator,
        NativeFn::IteratorProtoIterator,
    );
    let array_iterator_prototype = raw_object(vm, Some(iterator_prototype));
    vm.session.realm.array_iterator_prototype = array_iterator_prototype;
    install_function(vm, array_iterator_prototype, NativeFn::ArrayIteratorNext);
}

pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    if let [JsValue::Number(length)] = args {
        if length.fract() != 0.0 || *length < 0.0 || *length > f64::from(u32::MAX - 1) {
            return Err(vm.range_error("Invalid array length"));
        }
        let array = vm.new_array(Vec::new())?;
        let JsValue::Ref(id) = &array else { unreachable!() };
        if let Err(err) = vm.set_array_length(*id, *length) {
            array.drop_with(&mut vm.session.heap);
            return Err(err);
        }
        return Ok(array);
    }
    let items: Vec<JsValue> = args.iter().map(|a| a.clone_with_heap(&vm.session.heap)).collect();
    vm.new_array(items)
}

pub(super) fn is_array(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(vm.is_array(arg(args, 0))))
}

pub(super) fn from(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let source = arg(args, 0);
    let items = if source.is_object()
        && !vm.is_array(source)
        && !vm.has_property(
            match source {
                JsValue::Ref(id) => *id,
                _ => unreachable!(),
            },
            PropertyKey::Symbol(WellKnownSymbol::Iterator.id()),
        ) {
        super::function::array_like_to_vec(vm, source.clone_with_heap(&vm.session.heap))?
    } else {
        vm.iterate_to_vec(source.clone_with_heap(&vm.session.heap))?
    };
    let map = arg(args, 1);
    if *map == JsValue::Undefined {
        return vm.new_array(items);
    }
    require_callable(vm, map)?;
    let mut mapped = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let result = vm.call(
            map.clone_with_heap(&vm.session.heap),
            JsValue::Undefined,
            vec![item, JsValue::Number(index as f64)],
        );
        match result {
            Ok(value) => mapped.push(value),
            Err(err) => {
                for value in mapped {
                    value.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        }
    }
    vm.new_array(mapped)
}

pub(super) fn of(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let items: Vec<JsValue> = args.iter().map(|a| a.clone_with_heap(&vm.session.heap)).collect();
    vm.new_array(items)
}

/// The array a method was called on.
fn this_array(vm: &mut Vm<'_>, this: &JsValue, method: &str) -> RunResult<HeapId> {
    match this {
        JsValue::Ref(id)
            if matches!(
                vm.object(*id).map(|o| &o.class),
                Some(ObjectClass::Array | ObjectClass::Arguments)
            ) =>
        {
            Ok(*id)
        }
        _ => Err(vm.type_error(&format!("Array.prototype.{method} called on a non-array"))),
    }
}

fn len(vm: &Vm<'_>, array: HeapId) -> usize {
    vm.object(array).map_or(0, |o| o.elements.len())
}

/// Element `index` as a value (holes read as `undefined`).
fn element(vm: &Vm<'_>, array: HeapId, index: usize) -> JsValue {
    vm.object(array)
        .and_then(|o| o.elements.get(index))
        .map_or(JsValue::Undefined, |v| match v {
            JsValue::Hole => JsValue::Undefined,
            other => other.clone_with_heap(&vm.session.heap),
        })
}

/// Calls `callback(element, index, array)`.
fn call_back(
    vm: &mut Vm<'_>,
    callback: &JsValue,
    this_arg: &JsValue,
    array: HeapId,
    index: usize,
) -> RunResult<JsValue> {
    let item = element(vm, array, index);
    vm.session.heap.inc_ref(array);
    vm.call(
        callback.clone_with_heap(&vm.session.heap),
        this_arg.clone_with_heap(&vm.session.heap),
        vec![item, JsValue::Number(index as f64), JsValue::Ref(array)],
    )
}

pub(super) fn push(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "push")?;
    for value in args {
        let value = value.clone_with_heap(&vm.session.heap);
        vm.array_push(array, value)?;
    }
    Ok(JsValue::Number(len(vm, array) as f64))
}

pub(super) fn pop(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "pop")?;
    let popped = vm.object_mut(array).and_then(|o| o.elements.pop());
    Ok(match popped {
        Some(JsValue::Hole) | None => JsValue::Undefined,
        Some(value) => value,
    })
}

pub(super) fn shift(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "shift")?;
    let object = vm.object_mut(array).expect("array");
    if object.elements.is_empty() {
        return Ok(JsValue::Undefined);
    }
    let first = object.elements.remove(0);
    Ok(if first == JsValue::Hole {
        JsValue::Undefined
    } else {
        first
    })
}

pub(super) fn unshift(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "unshift")?;
    vm.tracker().check_allocation(args.len() * 16)?;
    let items: Vec<JsValue> = args.iter().map(|a| a.clone_with_heap(&vm.session.heap)).collect();
    let object = vm.object_mut(array).expect("array");
    object.elements.splice(0..0, items);
    Ok(JsValue::Number(len(vm, array) as f64))
}

pub(super) fn slice(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "slice")?;
    let length = len(vm, array);
    let start = relative_index(integer_arg(vm, args, 0, 0.0)?, length);
    let end = relative_index(integer_arg(vm, args, 1, length as f64)?, length);
    let items: Vec<JsValue> = (start..end.max(start)).map(|i| raw_element(vm, array, i)).collect();
    vm.new_array(items)
}

/// Element `index` keeping holes as holes.
fn raw_element(vm: &Vm<'_>, array: HeapId, index: usize) -> JsValue {
    vm.object(array)
        .and_then(|o| o.elements.get(index))
        .map_or(JsValue::Hole, |v| v.clone_with_heap(&vm.session.heap))
}

pub(super) fn splice(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "splice")?;
    let length = len(vm, array);
    let start = relative_index(integer_arg(vm, args, 0, 0.0)?, length);
    let delete_count = match args.len() {
        0 => 0,
        1 => length - start,
        _ => {
            let count = integer_arg(vm, args, 1, 0.0)?;
            to_usize(count).min(length - start)
        }
    };
    let inserted: Vec<JsValue> = args
        .iter()
        .skip(2)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    vm.tracker().check_allocation(inserted.len() * 16)?;
    let object = vm.object_mut(array).expect("array");
    let removed: Vec<JsValue> = object.elements.splice(start..start + delete_count, inserted).collect();
    vm.new_array(removed)
}

/// The search for `indexOf` / `lastIndexOf` / `includes`.
fn search(vm: &mut Vm<'_>, array: HeapId, target: &JsValue, from: usize, reverse: bool, zero: bool) -> Option<usize> {
    let length = len(vm, array);
    let indices: Box<dyn Iterator<Item = usize>> = if reverse {
        Box::new((0..=from.min(length.saturating_sub(1))).rev())
    } else {
        Box::new(from..length)
    };
    for index in indices {
        let candidate = raw_element(vm, array, index);
        let hit = if candidate == JsValue::Hole {
            zero && target.is_nullish() && *target == JsValue::Undefined
        } else if zero {
            vm.same_value_zero(&candidate, target)
        } else {
            vm.strict_equals(&candidate, target)
        };
        candidate.drop_with(&mut vm.session.heap);
        if hit {
            return Some(index);
        }
    }
    None
}

pub(super) fn index_of(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "indexOf")?;
    let length = len(vm, array);
    let from = relative_index(integer_arg(vm, args, 1, 0.0)?, length);
    let found = search(vm, array, arg(args, 0), from, false, false);
    Ok(JsValue::Number(found.map_or(-1.0, |i| i as f64)))
}

pub(super) fn last_index_of(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "lastIndexOf")?;
    let length = len(vm, array);
    if length == 0 {
        return Ok(JsValue::Number(-1.0));
    }
    let from = match args.get(1) {
        None => length - 1,
        Some(value) => {
            let n = vm.coerce_integer(value)?;
            if n < 0.0 {
                let resolved = length as f64 + n;
                if resolved < 0.0 {
                    return Ok(JsValue::Number(-1.0));
                }
                to_usize(resolved)
            } else {
                to_usize(n).min(length - 1)
            }
        }
    };
    let found = search(vm, array, arg(args, 0), from, true, false);
    Ok(JsValue::Number(found.map_or(-1.0, |i| i as f64)))
}

pub(super) fn includes(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "includes")?;
    let length = len(vm, array);
    let from = relative_index(integer_arg(vm, args, 1, 0.0)?, length);
    Ok(JsValue::Bool(
        search(vm, array, arg(args, 0), from, false, true).is_some(),
    ))
}

pub(super) fn join(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "join")?;
    let separator = match arg(args, 0) {
        JsValue::Undefined => JsString::from(","),
        other => vm.coerce_string(other)?,
    };
    let text = join_elements(vm, array, &separator)?;
    vm.alloc_string(text)
}

/// `join`'s body, shared with `toString`; nullish elements are empty.
fn join_elements(vm: &mut Vm<'_>, array: HeapId, separator: &JsString) -> RunResult<JsString> {
    let length = len(vm, array);
    let mut units: Vec<u16> = Vec::new();
    for index in 0..length {
        if index > 0 {
            units.extend(separator.units());
        }
        let item = element(vm, array, index);
        if item.is_nullish() {
            item.drop_with(&mut vm.session.heap);
        } else {
            let text = vm.coerce_string(&item);
            item.drop_with(&mut vm.session.heap);
            let text = text?;
            vm.tracker().check_growth(units.len(), units.capacity(), 2)?;
            units.extend(text.units());
        }
        vm.tracker().check_memory_time_every(index)?;
    }
    Ok(JsString::from_units(&units))
}

pub(super) fn to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "toString")?;
    let text = join_elements(vm, array, &JsString::from(","))?;
    vm.alloc_string(text)
}

pub(super) fn map(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "map")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    let length = len(vm, array);
    let mut results = Vec::with_capacity(length);
    for index in 0..length {
        if raw_element_is_hole(vm, array, index) {
            results.push(JsValue::Hole);
            continue;
        }
        match call_back(vm, callback, this_arg, array, index) {
            Ok(value) => results.push(value),
            Err(err) => {
                for value in results {
                    value.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        }
    }
    vm.new_array(results)
}

fn raw_element_is_hole(vm: &Vm<'_>, array: HeapId, index: usize) -> bool {
    vm.object(array)
        .and_then(|o| o.elements.get(index))
        .is_none_or(|v| *v == JsValue::Hole)
}

pub(super) fn filter(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "filter")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    let mut results: Vec<JsValue> = Vec::new();
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let keep = match call_back(vm, callback, this_arg, array, index) {
            Ok(value) => {
                let keep = vm.to_boolean(&value);
                value.drop_with(&mut vm.session.heap);
                keep
            }
            Err(err) => {
                for value in results {
                    value.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        };
        if keep {
            results.push(element(vm, array, index));
        }
    }
    vm.new_array(results)
}

pub(super) fn for_each(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "forEach")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let result = call_back(vm, callback, this_arg, array, index)?;
        result.drop_with(&mut vm.session.heap);
    }
    Ok(JsValue::Undefined)
}

/// `reduce` / `reduceRight`.
fn reduce_impl(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], right: bool) -> RunResult<JsValue> {
    let name = if right { "reduceRight" } else { "reduce" };
    let array = this_array(vm, this, name)?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let length = len(vm, array);
    let mut indices: Vec<usize> = (0..length).filter(|i| !raw_element_is_hole(vm, array, *i)).collect();
    if right {
        indices.reverse();
    }
    let mut iter = indices.into_iter();
    let mut accumulator = match args.get(1) {
        Some(initial) => initial.clone_with_heap(&vm.session.heap),
        None => match iter.next() {
            Some(index) => element(vm, array, index),
            None => return Err(vm.type_error("Reduce of empty array with no initial value")),
        },
    };
    for index in iter {
        let item = element(vm, array, index);
        vm.session.heap.inc_ref(array);
        accumulator = vm.call(
            callback.clone_with_heap(&vm.session.heap),
            JsValue::Undefined,
            vec![accumulator, item, JsValue::Number(index as f64), JsValue::Ref(array)],
        )?;
    }
    Ok(accumulator)
}

pub(super) fn reduce(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    reduce_impl(vm, this, args, false)
}

pub(super) fn reduce_right(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    reduce_impl(vm, this, args, true)
}

/// The index of the first (or last) element the predicate accepts.
fn find_impl(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], name: &str, last: bool) -> RunResult<Option<usize>> {
    let array = this_array(vm, this, name)?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    let length = len(vm, array);
    let indices: Box<dyn Iterator<Item = usize>> = if last {
        Box::new((0..length).rev())
    } else {
        Box::new(0..length)
    };
    for index in indices {
        let result = call_back(vm, callback, this_arg, array, index)?;
        let hit = vm.to_boolean(&result);
        result.drop_with(&mut vm.session.heap);
        if hit {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

pub(super) fn find(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "find")?;
    Ok(find_impl(vm, this, args, "find", false)?.map_or(JsValue::Undefined, |i| element(vm, array, i)))
}

pub(super) fn find_index(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Number(
        find_impl(vm, this, args, "findIndex", false)?.map_or(-1.0, |i| i as f64),
    ))
}

pub(super) fn find_last(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "findLast")?;
    Ok(find_impl(vm, this, args, "findLast", true)?.map_or(JsValue::Undefined, |i| element(vm, array, i)))
}

pub(super) fn find_last_index(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Number(
        find_impl(vm, this, args, "findLastIndex", true)?.map_or(-1.0, |i| i as f64),
    ))
}

pub(super) fn some(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "some")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let result = call_back(vm, callback, this_arg, array, index)?;
        let hit = vm.to_boolean(&result);
        result.drop_with(&mut vm.session.heap);
        if hit {
            return Ok(JsValue::Bool(true));
        }
    }
    Ok(JsValue::Bool(false))
}

pub(super) fn every(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "every")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let result = call_back(vm, callback, this_arg, array, index)?;
        let hit = vm.to_boolean(&result);
        result.drop_with(&mut vm.session.heap);
        if !hit {
            return Ok(JsValue::Bool(false));
        }
    }
    Ok(JsValue::Bool(true))
}

pub(super) fn concat(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "concat")?;
    let mut items: Vec<JsValue> = (0..len(vm, array)).map(|i| raw_element(vm, array, i)).collect();
    for value in args {
        if vm.is_array(value) {
            let JsValue::Ref(id) = value else { unreachable!() };
            let extra = len(vm, *id);
            vm.tracker().check_allocation(extra * 16)?;
            items.extend((0..extra).map(|i| raw_element(vm, *id, i)));
        } else {
            items.push(value.clone_with_heap(&vm.session.heap));
        }
    }
    vm.new_array(items)
}

pub(super) fn reverse(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "reverse")?;
    vm.object_mut(array).expect("array").elements.reverse();
    Ok(this.clone_with_heap(&vm.session.heap))
}

/// `sort`: stable merge sort with the comparator or the string order
/// default; `undefined`s and holes sort last.
pub(super) fn sort(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "sort")?;
    let comparator = arg(args, 0);
    require_comparator(vm, comparator)?;
    let elements: Vec<JsValue> = mem::take(&mut vm.object_mut(array).expect("array").elements);
    let mut holes = 0;
    let mut undefineds = 0;
    let mut items = Vec::with_capacity(elements.len());
    for value in elements {
        match value {
            JsValue::Hole => holes += 1,
            JsValue::Undefined => undefineds += 1,
            other => items.push(other),
        }
    }
    let result = if *comparator == JsValue::Undefined {
        let mut keyed = Vec::with_capacity(items.len());
        for item in items {
            match vm.coerce_string(&item) {
                Ok(key) => keyed.push((key, item)),
                Err(err) => {
                    item.drop_with(&mut vm.session.heap);
                    for (_, value) in keyed {
                        value.drop_with(&mut vm.session.heap);
                    }
                    return Err(err);
                }
            }
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(keyed.into_iter().map(|(_, value)| value).collect::<Vec<_>>())
    } else {
        merge_sort(vm, items, comparator)
    };
    let mut sorted = result?;
    sorted.extend(iter::repeat_with(|| JsValue::Undefined).take(undefineds));
    sorted.extend(iter::repeat_with(|| JsValue::Hole).take(holes));
    vm.object_mut(array).expect("array").elements = sorted;
    Ok(this.clone_with_heap(&vm.session.heap))
}

/// `sort`'s argument must be callable or absent, with V8's wording.
fn require_comparator(vm: &mut Vm<'_>, comparator: &JsValue) -> RunResult<()> {
    if *comparator == JsValue::Undefined || vm.is_callable(comparator) {
        return Ok(());
    }
    let shown = vm.inspect_short(comparator)?;
    Err(vm.type_error(&format!(
        "The comparison function must be either a function or undefined: {shown}"
    )))
}

/// A stable sort that tolerates a comparator that throws (the items are
/// returned to the array either way).
fn merge_sort(vm: &mut Vm<'_>, items: Vec<JsValue>, comparator: &JsValue) -> RunResult<Vec<JsValue>> {
    if items.len() <= 1 {
        return Ok(items);
    }
    let mut items = items;
    let right = items.split_off(items.len() / 2);
    let left = merge_sort(vm, items, comparator);
    let left = match left {
        Ok(left) => left,
        Err(err) => {
            for value in right {
                value.drop_with(&mut vm.session.heap);
            }
            return Err(err);
        }
    };
    let right = match merge_sort(vm, right, comparator) {
        Ok(right) => right,
        Err(err) => {
            for value in left {
                value.drop_with(&mut vm.session.heap);
            }
            return Err(err);
        }
    };
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    loop {
        match (left.peek(), right.peek()) {
            (Some(a), Some(b)) => {
                let ordering = vm.call(
                    comparator.clone_with_heap(&vm.session.heap),
                    JsValue::Undefined,
                    vec![a.clone_with_heap(&vm.session.heap), b.clone_with_heap(&vm.session.heap)],
                );
                let ordering = match ordering {
                    Ok(value) => {
                        let n = vm.coerce_number(&value);
                        value.drop_with(&mut vm.session.heap);
                        n
                    }
                    Err(err) => Err(err),
                };
                match ordering {
                    Ok(n) if n > 0.0 => merged.push(right.next().expect("peeked")),
                    Ok(_) => merged.push(left.next().expect("peeked")),
                    Err(err) => {
                        for value in merged.into_iter().chain(left).chain(right) {
                            value.drop_with(&mut vm.session.heap);
                        }
                        return Err(err);
                    }
                }
            }
            (Some(_), None) => merged.extend(left.by_ref()),
            (None, Some(_)) => merged.extend(right.by_ref()),
            (None, None) => break,
        }
    }
    Ok(merged)
}

pub(super) fn flat(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "flat")?;
    let depth = integer_arg(vm, args, 0, 1.0)?;
    let mut out = Vec::new();
    flatten_into(vm, array, depth, &mut out)?;
    vm.new_array(out)
}

fn flatten_into(vm: &mut Vm<'_>, array: HeapId, depth: f64, out: &mut Vec<JsValue>) -> RunResult<()> {
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let item = element(vm, array, index);
        if depth >= 1.0 && vm.is_array(&item) {
            let JsValue::Ref(inner) = &item else { unreachable!() };
            let inner = *inner;
            let result = flatten_into(vm, inner, depth - 1.0, out);
            item.drop_with(&mut vm.session.heap);
            result?;
        } else {
            vm.tracker().check_growth(out.len(), out.capacity(), 16)?;
            out.push(item);
        }
    }
    Ok(())
}

pub(super) fn flat_map(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "flatMap")?;
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    let mut out: Vec<JsValue> = Vec::new();
    for index in 0..len(vm, array) {
        if raw_element_is_hole(vm, array, index) {
            continue;
        }
        let mapped = match call_back(vm, callback, this_arg, array, index) {
            Ok(value) => value,
            Err(err) => {
                for value in out {
                    value.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        };
        if vm.is_array(&mapped) {
            let JsValue::Ref(inner) = &mapped else { unreachable!() };
            let inner = *inner;
            let result = flatten_into(vm, inner, 0.0, &mut out);
            mapped.drop_with(&mut vm.session.heap);
            if let Err(err) = result {
                for value in out {
                    value.drop_with(&mut vm.session.heap);
                }
                return Err(err);
            }
        } else {
            out.push(mapped);
        }
    }
    vm.new_array(out)
}

pub(super) fn fill(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "fill")?;
    let length = len(vm, array);
    let start = relative_index(integer_arg(vm, args, 1, 0.0)?, length);
    let end = relative_index(integer_arg(vm, args, 2, length as f64)?, length);
    for index in start..end.max(start) {
        let value = arg(args, 0).clone_with_heap(&vm.session.heap);
        let object = vm.object_mut(array).expect("array");
        let old = mem::replace(&mut object.elements[index], value);
        old.drop_with(&mut vm.session.heap);
    }
    Ok(this.clone_with_heap(&vm.session.heap))
}

/// `copyWithin(target, start, end)`: moves a slice within the array in place.
pub(super) fn copy_within(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "copyWithin")?;
    let length = len(vm, array);
    let target = relative_index(integer_arg(vm, args, 0, 0.0)?, length);
    let start = relative_index(integer_arg(vm, args, 1, 0.0)?, length);
    let end = relative_index(integer_arg(vm, args, 2, length as f64)?, length);
    let count = end.saturating_sub(start).min(length - target);
    let moved: Vec<JsValue> = (start..start + count).map(|i| raw_element(vm, array, i)).collect();
    for (offset, value) in moved.into_iter().enumerate() {
        let object = vm.object_mut(array).expect("array");
        let old = mem::replace(&mut object.elements[target + offset], value);
        old.drop_with(&mut vm.session.heap);
    }
    Ok(this.clone_with_heap(&vm.session.heap))
}

/// The elements as a fresh vector with holes read as `undefined`, for the
/// copying methods (`toSorted`, `toReversed`, `toSpliced`, `with`).
fn copied_elements(vm: &mut Vm<'_>, array: HeapId) -> RunResult<Vec<JsValue>> {
    let length = len(vm, array);
    vm.tracker().check_allocation(length * 16)?;
    Ok((0..length).map(|i| element(vm, array, i)).collect())
}

pub(super) fn to_sorted(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "toSorted")?;
    let comparator = arg(args, 0);
    require_comparator(vm, comparator)?;
    let items = copied_elements(vm, array)?;
    let copy = vm.new_array(items)?;
    let sorted = sort(vm, &copy, args, false);
    match sorted {
        Ok(result) => {
            result.drop_with(&mut vm.session.heap);
            Ok(copy)
        }
        Err(err) => {
            copy.drop_with(&mut vm.session.heap);
            Err(err)
        }
    }
}

pub(super) fn to_reversed(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "toReversed")?;
    let mut items = copied_elements(vm, array)?;
    items.reverse();
    vm.new_array(items)
}

pub(super) fn to_spliced(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "toSpliced")?;
    let length = len(vm, array);
    let start = relative_index(integer_arg(vm, args, 0, 0.0)?, length);
    let skip = match args.len() {
        0 => 0,
        1 => length - start,
        _ => to_usize(integer_arg(vm, args, 1, 0.0)?).min(length - start),
    };
    let mut items = copied_elements(vm, array)?;
    let inserted: Vec<JsValue> = args
        .iter()
        .skip(2)
        .map(|a| a.clone_with_heap(&vm.session.heap))
        .collect();
    let removed: Vec<JsValue> = items.splice(start..start + skip, inserted).collect();
    vm.drop_all(JsValue::Undefined, removed);
    vm.new_array(items)
}

/// `with(index, value)`: a copy with one element replaced.
pub(super) fn with(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "with")?;
    let length = len(vm, array) as f64;
    let relative = integer_arg(vm, args, 0, 0.0)?;
    let index = if relative < 0.0 { length + relative } else { relative };
    if index < 0.0 || index >= length {
        let shown = number_to_string(relative);
        return Err(vm.range_error(&format!("Invalid index : {shown}")));
    }
    let mut items = copied_elements(vm, array)?;
    let old = mem::replace(
        &mut items[to_usize(index)],
        arg(args, 1).clone_with_heap(&vm.session.heap),
    );
    old.drop_with(&mut vm.session.heap);
    vm.new_array(items)
}

pub(super) fn at(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "at")?;
    let length = len(vm, array) as f64;
    let index = integer_arg(vm, args, 0, 0.0)?;
    let index = if index < 0.0 { length + index } else { index };
    if index < 0.0 || index >= length {
        return Ok(JsValue::Undefined);
    }
    Ok(element(vm, array, to_usize(index)))
}

pub(super) fn keys(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "keys")?;
    Ok(vm.array_iterator(array, IterKind::Keys))
}

pub(super) fn values(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "values")?;
    Ok(vm.array_iterator(array, IterKind::Values))
}

pub(super) fn entries(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let array = this_array(vm, this, "entries")?;
    Ok(vm.array_iterator(array, IterKind::Entries))
}

/// `%ArrayIteratorPrototype%.next` (and the string iterator's): a
/// `{ value, done }` result object.
pub(super) fn iterator_next(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let JsValue::Ref(id) = this else {
        return Err(vm.type_error("next method called on incompatible receiver"));
    };
    let (value, done) = match vm.builtin_iterator_next(*id)? {
        IteratorStep::Value(value) => (value, false),
        IteratorStep::Done => (JsValue::Undefined, true),
        IteratorStep::NotBuiltin => return Err(vm.type_error("next method called on incompatible receiver")),
    };
    Ok(vm.iter_result(value, done))
}

/// `%IteratorPrototype%[Symbol.iterator]`: returns the receiver.
pub(super) fn iterator_self(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(this.clone_with_heap(&vm.session.heap))
}
