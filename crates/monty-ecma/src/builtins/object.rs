//! `Object` and `Object.prototype`.

use std::mem;

use super::{NativeFn, arg, install_constructor, install_function, plain_object, this_object};
use crate::{
    error::RunResult,
    heap::{HeapId, ObjectClass, PropFlags, PropertyKey, Slot},
    intern::{StaticAtom, WellKnownSymbol},
    string::JsString,
    value::JsValue,
    vm::{Vm, number::index_u32},
};

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId, prototype: HeapId) {
    for native in [
        NativeFn::ObjectProtoHasOwnProperty,
        NativeFn::ObjectProtoToString,
        NativeFn::ObjectProtoValueOf,
        NativeFn::ObjectProtoIsPrototypeOf,
        NativeFn::ObjectProtoPropertyIsEnumerable,
    ] {
        install_function(vm, prototype, native);
    }
    let constructor = install_constructor(vm, global, NativeFn::Object, prototype);
    for native in [
        NativeFn::ObjectKeys,
        NativeFn::ObjectValues,
        NativeFn::ObjectEntries,
        NativeFn::ObjectAssign,
        NativeFn::ObjectFreeze,
        NativeFn::ObjectIsFrozen,
        NativeFn::ObjectCreate,
        NativeFn::ObjectGetPrototypeOf,
        NativeFn::ObjectSetPrototypeOf,
        NativeFn::ObjectDefineProperty,
        NativeFn::ObjectGetOwnPropertyNames,
        NativeFn::ObjectFromEntries,
        NativeFn::ObjectHasOwn,
        NativeFn::ObjectIs,
        NativeFn::ObjectGetOwnPropertyDescriptor,
        NativeFn::ObjectGetOwnPropertyDescriptors,
        NativeFn::ObjectGetOwnPropertySymbols,
        NativeFn::ObjectDefineProperties,
        NativeFn::ObjectSeal,
        NativeFn::ObjectIsSealed,
        NativeFn::ObjectPreventExtensions,
        NativeFn::ObjectIsExtensible,
        NativeFn::ObjectGroupBy,
    ] {
        install_function(vm, constructor, native);
    }
}

pub(super) fn constructor(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    match arg(args, 0) {
        JsValue::Undefined | JsValue::Null => Ok(vm.new_object()),
        value => to_object(vm, value),
    }
}

/// `ToObject`: objects pass through, primitives are boxed.
pub(crate) fn to_object(vm: &mut Vm<'_>, value: &JsValue) -> RunResult<JsValue> {
    let (proto, class) = match value {
        JsValue::Ref(_) => return Ok(value.clone_with_heap(&vm.session.heap)),
        JsValue::Bool(b) => (vm.session.realm.boolean_prototype, ObjectClass::BoxedBool(*b)),
        JsValue::Number(n) => (vm.session.realm.number_prototype, ObjectClass::BoxedNumber(*n)),
        JsValue::Atom(_) | JsValue::Str(_) => (
            vm.session.realm.string_prototype,
            ObjectClass::BoxedString(vm.string_of(value).expect("string")),
        ),
        JsValue::Symbol(s) => (vm.session.realm.symbol_prototype, ObjectClass::BoxedSymbol(*s)),
        JsValue::BigInt(_) => return Err(vm.type_error("BigInt is not supported")),
        JsValue::Undefined | JsValue::Null | JsValue::Hole => {
            let what = if *value == JsValue::Null { "null" } else { "undefined" };
            return Err(vm.type_error(&format!("Cannot convert {what} to object")));
        }
    };
    Ok(JsValue::Ref(vm.new_class_object(proto, class)))
}

/// The object an `Object.*` static operates on: primitives are boxed,
/// nullish throws.
fn object_arg(vm: &mut Vm<'_>, args: &[JsValue], method: &str) -> RunResult<JsValue> {
    let value = arg(args, 0);
    if value.is_nullish() {
        let what = if *value == JsValue::Null { "null" } else { "undefined" };
        return Err(vm.type_error(&format!("Cannot convert {what} to object")));
    }
    let _ = method;
    to_object(vm, value)
}

/// Own enumerable string keys as values.
fn enumerable_keys(vm: &mut Vm<'_>, object: HeapId) -> Vec<(PropertyKey, JsValue)> {
    vm.own_keys(object, true)
        .into_iter()
        .map(|key| {
            let value = vm.key_to_value(key);
            (key, value)
        })
        .collect()
}

pub(super) fn keys(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "keys")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let keys: Vec<JsValue> = enumerable_keys(vm, *id).into_iter().map(|(_, v)| v).collect();
    object.drop_with(&mut vm.session.heap);
    vm.new_array(keys)
}

pub(super) fn values(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "values")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let mut values = Vec::new();
    for (key, name) in enumerable_keys(vm, *id) {
        name.drop_with(&mut vm.session.heap);
        values.push(vm.get(&object, key)?);
    }
    object.drop_with(&mut vm.session.heap);
    vm.new_array(values)
}

pub(super) fn entries(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "entries")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let mut entries = Vec::new();
    for (key, name) in enumerable_keys(vm, *id) {
        let value = vm.get(&object, key)?;
        entries.push(vm.new_array(vec![name, value])?);
    }
    object.drop_with(&mut vm.session.heap);
    vm.new_array(entries)
}

pub(super) fn assign(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let target = object_arg(vm, args, "assign")?;
    let JsValue::Ref(target_id) = &target else {
        unreachable!()
    };
    let target_id = *target_id;
    for source in args.iter().skip(1) {
        if source.is_nullish() {
            continue;
        }
        let JsValue::Ref(source_id) = source else {
            vm.copy_data_properties(target_id, source)?;
            continue;
        };
        for (key, name) in enumerable_keys(vm, *source_id) {
            name.drop_with(&mut vm.session.heap);
            let value = vm.get(source, key)?;
            vm.set(&target, key, value)?;
        }
    }
    Ok(target)
}

pub(super) fn freeze(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = arg(args, 0);
    if let JsValue::Ref(id) = value
        && let Some(object) = vm.object_mut(*id)
    {
        object.extensible = false;
        for slot in object.props.values_mut() {
            match slot {
                Slot::Data { flags, .. } => *flags = flags.read_only().sealed(),
                Slot::Accessor { flags, .. } => *flags = flags.sealed(),
            }
        }
        // array elements become read-only by moving them into named slots
        if matches!(object.class, ObjectClass::Array) {
            let elements = mem::take(&mut object.elements);
            object.class = ObjectClass::Ordinary;
            let proto = object.proto;
            let _ = proto;
            let count = elements.len();
            for (index, element) in elements.into_iter().enumerate() {
                if element == JsValue::Hole {
                    continue;
                }
                vm.define_with_flags(*id, PropertyKey::Index(index_u32(index)), element, PropFlags::NONE);
            }
            vm.define_with_flags(
                *id,
                PropertyKey::Atom(StaticAtom::Length.id()),
                JsValue::Number(count as f64),
                PropFlags::NONE,
            );
            let frozen = vm.object_mut(*id).expect("object");
            frozen.class = ObjectClass::Array;
            // keep it an array for `Array.isArray`; its elements now live in named slots
            frozen.elements = Vec::new();
            let _ = frozen;
            return Ok(value.clone_with_heap(&vm.session.heap));
        }
    }
    Ok(value.clone_with_heap(&vm.session.heap))
}

pub(super) fn is_frozen(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let frozen = match arg(args, 0) {
        JsValue::Ref(id) => vm.object(*id).is_some_and(|object| {
            !object.extensible
                && object.props.values().all(|slot| match slot {
                    Slot::Data { flags, .. } => !flags.writable() && !flags.configurable(),
                    Slot::Accessor { flags, .. } => !flags.configurable(),
                })
                && object.elements.is_empty()
        }),
        _ => true,
    };
    Ok(JsValue::Bool(frozen))
}

pub(super) fn create(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let proto = match arg(args, 0) {
        JsValue::Ref(id) => Some(*id),
        JsValue::Null => None,
        other => {
            let shown = vm.inspect_short(other)?;
            return Err(vm.type_error(&format!("Object prototype may only be an Object or null: {shown}")));
        }
    };
    Ok(vm.new_object_with_proto(proto))
}

pub(super) fn get_prototype_of(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "getPrototypeOf")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let proto = vm.get_prototype(*id);
    object.drop_with(&mut vm.session.heap);
    Ok(match proto {
        Some(proto) => {
            vm.session.heap.inc_ref(proto);
            JsValue::Ref(proto)
        }
        None => JsValue::Null,
    })
}

pub(super) fn set_prototype_of(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let target = arg(args, 0);
    let proto = match arg(args, 1) {
        JsValue::Ref(id) => Some(*id),
        JsValue::Null => None,
        other => {
            let shown = vm.inspect_short(other)?;
            return Err(vm.type_error(&format!("Object prototype may only be an Object or null: {shown}")));
        }
    };
    if let JsValue::Ref(id) = target {
        vm.set_prototype(*id, proto);
    }
    Ok(target.clone_with_heap(&vm.session.heap))
}

pub(super) fn define_property(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let target = arg(args, 0);
    let JsValue::Ref(id) = target else {
        return Err(vm.type_error("Object.defineProperty called on non-object"));
    };
    let id = *id;
    let key = vm.coerce_key(arg(args, 1).clone_with_heap(&vm.session.heap))?;
    let descriptor = arg(args, 2);
    if !descriptor.is_object() {
        let shown = vm.inspect_short(descriptor)?;
        return Err(vm.type_error(&format!("Property description must be an object: {shown}")));
    }
    let field = |vm: &mut Vm<'_>, name: StaticAtom| -> RunResult<Option<JsValue>> {
        let JsValue::Ref(desc) = descriptor else { unreachable!() };
        if vm.has_property(*desc, PropertyKey::Atom(name.id())) {
            vm.get_atom(descriptor, name.id()).map(Some)
        } else {
            Ok(None)
        }
    };
    let getter = field(vm, StaticAtom::Get)?;
    let setter = field(vm, StaticAtom::Set)?;
    let enumerable = field(vm, StaticAtom::Enumerable)?;
    let enumerable = truthy_field(vm, enumerable);
    let configurable = field(vm, StaticAtom::Configurable)?;
    let configurable = truthy_field(vm, configurable);
    let writable = field(vm, StaticAtom::Writable)?;
    let writable = truthy_field(vm, writable);
    // attributes default to false for a new property and are kept for an existing one
    let existing = vm.object(id).and_then(|o| o.props.get(&key)).map(Slot::flags);
    let mut flags = existing.unwrap_or(PropFlags::NONE);
    let mut bits = 0u8;
    if enumerable.unwrap_or(flags.enumerable()) {
        bits |= 2;
    }
    if configurable.unwrap_or(flags.configurable()) {
        bits |= 4;
    }
    if getter.is_some() || setter.is_some() {
        let existing_getter = match vm.object(id).and_then(|o| o.props.get(&key)) {
            Some(Slot::Accessor { getter, .. }) => getter.clone_with_heap(&vm.session.heap),
            _ => JsValue::Undefined,
        };
        let existing_setter = match vm.object(id).and_then(|o| o.props.get(&key)) {
            Some(Slot::Accessor { setter, .. }) => setter.clone_with_heap(&vm.session.heap),
            _ => JsValue::Undefined,
        };
        flags = PropFlags::from_bits(bits);
        vm.define_slot(
            id,
            key,
            Slot::Accessor {
                getter: getter.unwrap_or(existing_getter),
                setter: setter.unwrap_or(existing_setter),
                flags,
            },
        );
    } else {
        if writable.unwrap_or(flags.writable()) {
            bits |= 1;
        }
        let value = match field(vm, StaticAtom::Value)? {
            Some(value) => value,
            None => match vm.object(id).and_then(|o| o.props.get(&key)) {
                Some(Slot::Data { value, .. }) => value.clone_with_heap(&vm.session.heap),
                _ => JsValue::Undefined,
            },
        };
        vm.define_with_flags(id, key, value, PropFlags::from_bits(bits));
    }
    Ok(target.clone_with_heap(&vm.session.heap))
}

/// A descriptor attribute's truthiness, `None` when the field is absent.
fn truthy_field(vm: &mut Vm<'_>, value: Option<JsValue>) -> Option<bool> {
    value.map(|v| {
        let b = vm.to_boolean(&v);
        v.drop_with(&mut vm.session.heap);
        b
    })
}

/// `Object.getOwnPropertyDescriptor`: a descriptor object, or `undefined`.
pub(super) fn get_own_property_descriptor(
    vm: &mut Vm<'_>,
    _: &JsValue,
    args: &[JsValue],
    _: bool,
) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "getOwnPropertyDescriptor")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let id = *id;
    let key = vm.coerce_key(arg(args, 1).clone_with_heap(&vm.session.heap))?;
    let descriptor = describe_property(vm, id, key)?;
    object.drop_with(&mut vm.session.heap);
    Ok(descriptor.unwrap_or(JsValue::Undefined))
}

/// `Object.getOwnPropertyDescriptors`: every own property's descriptor.
pub(super) fn get_own_property_descriptors(
    vm: &mut Vm<'_>,
    _: &JsValue,
    args: &[JsValue],
    _: bool,
) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "getOwnPropertyDescriptors")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let id = *id;
    let result = plain_object(vm);
    for key in vm.own_keys(id, false) {
        if let Some(descriptor) = describe_property(vm, id, key)? {
            vm.define_data(result, key, descriptor);
        }
    }
    object.drop_with(&mut vm.session.heap);
    Ok(JsValue::Ref(result))
}

/// The `{ value, writable, enumerable, configurable }` or
/// `{ get, set, enumerable, configurable }` object of an own property.
fn describe_property(vm: &mut Vm<'_>, object: HeapId, key: PropertyKey) -> RunResult<Option<JsValue>> {
    let slot = match vm.object(object).and_then(|o| o.props.get(&key)) {
        Some(Slot::Data { value, flags }) => Some((Some(value.clone_with_heap(&vm.session.heap)), None, *flags)),
        Some(Slot::Accessor { getter, setter, flags }) => Some((
            None,
            Some((
                getter.clone_with_heap(&vm.session.heap),
                setter.clone_with_heap(&vm.session.heap),
            )),
            *flags,
        )),
        None => None,
    };
    let slot = if let Some(slot) = slot {
        slot
    } else {
        // virtual properties: array elements and `length`
        let Some(target) = vm.object(object) else {
            return Ok(None);
        };
        if !vm.has_own(target, key) {
            return Ok(None);
        }
        let is_length = matches!(key, PropertyKey::Atom(atom) if atom == StaticAtom::Length.id());
        let value = vm.get(&JsValue::Ref(object), key)?;
        let flags = if is_length { PropFlags::WRITABLE } else { PropFlags::ALL };
        (Some(value), None, flags)
    };
    let (value, accessors, flags) = slot;
    let descriptor = plain_object(vm);
    match (value, accessors) {
        (Some(value), _) => {
            define_field(vm, descriptor, "value", value);
            define_field(vm, descriptor, "writable", JsValue::Bool(flags.writable()));
        }
        (None, Some((getter, setter))) => {
            define_field(vm, descriptor, "get", getter);
            define_field(vm, descriptor, "set", setter);
        }
        (None, None) => unreachable!("a slot is data or accessor"),
    }
    define_field(vm, descriptor, "enumerable", JsValue::Bool(flags.enumerable()));
    define_field(vm, descriptor, "configurable", JsValue::Bool(flags.configurable()));
    Ok(Some(JsValue::Ref(descriptor)))
}

/// Defines a named data property on a descriptor object.
fn define_field(vm: &mut Vm<'_>, descriptor: HeapId, name: &str, value: JsValue) {
    let key = super::key(vm, name);
    vm.define_data(descriptor, key, value);
}

/// `Object.getOwnPropertySymbols`.
pub(super) fn get_own_property_symbols(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "getOwnPropertySymbols")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let symbols: Vec<JsValue> = vm
        .own_keys(*id, false)
        .into_iter()
        .filter_map(|key| match key {
            PropertyKey::Symbol(symbol) => Some(JsValue::Symbol(symbol)),
            _ => None,
        })
        .collect();
    object.drop_with(&mut vm.session.heap);
    vm.new_array(symbols)
}

/// `Object.defineProperties`: `defineProperty` for each own enumerable key of the descriptors object.
pub(super) fn define_properties(
    vm: &mut Vm<'_>,
    this: &JsValue,
    args: &[JsValue],
    construct: bool,
) -> RunResult<JsValue> {
    let target = arg(args, 0);
    if !target.is_object() {
        return Err(vm.type_error("Object.defineProperties called on non-object"));
    }
    let descriptors = object_arg(vm, &args[1.min(args.len())..], "defineProperties")?;
    let JsValue::Ref(descriptors_id) = &descriptors else {
        unreachable!()
    };
    for key in vm.own_keys(*descriptors_id, true) {
        let descriptor = vm.get(&descriptors, key)?;
        let key_value = vm.key_to_value(key);
        let outcome = define_property(
            vm,
            this,
            &[target.clone_with_heap(&vm.session.heap), key_value, descriptor],
            construct,
        );
        outcome?.drop_with(&mut vm.session.heap);
    }
    descriptors.drop_with(&mut vm.session.heap);
    Ok(target.clone_with_heap(&vm.session.heap))
}

/// `Object.seal`: no new properties, none deletable; values stay writable.
pub(super) fn seal(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = arg(args, 0);
    if let JsValue::Ref(id) = value
        && let Some(object) = vm.object_mut(*id)
    {
        object.extensible = false;
        for slot in object.props.values_mut() {
            match slot {
                Slot::Data { flags, .. } | Slot::Accessor { flags, .. } => *flags = flags.sealed(),
            }
        }
    }
    Ok(value.clone_with_heap(&vm.session.heap))
}

pub(super) fn is_sealed(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let sealed = match arg(args, 0) {
        JsValue::Ref(id) => vm.object(*id).is_some_and(|object| {
            !object.extensible
                && object.props.values().all(|slot| !slot.flags().configurable())
                && object.elements.is_empty()
        }),
        _ => true,
    };
    Ok(JsValue::Bool(sealed))
}

pub(super) fn prevent_extensions(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let value = arg(args, 0);
    if let JsValue::Ref(id) = value
        && let Some(object) = vm.object_mut(*id)
    {
        object.extensible = false;
    }
    Ok(value.clone_with_heap(&vm.session.heap))
}

pub(super) fn is_extensible(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let extensible = match arg(args, 0) {
        JsValue::Ref(id) => vm.object(*id).is_some_and(|object| object.extensible),
        _ => false,
    };
    Ok(JsValue::Bool(extensible))
}

pub(super) fn get_own_property_names(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "getOwnPropertyNames")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let mut names: Vec<JsValue> = vm
        .own_keys(*id, false)
        .into_iter()
        .filter(|key| !matches!(key, PropertyKey::Symbol(_)))
        .map(|key| vm.key_to_value(key))
        .collect();
    if matches!(
        vm.object(*id).map(|o| &o.class),
        Some(ObjectClass::Array | ObjectClass::Arguments)
    ) {
        names.push(JsValue::Atom(StaticAtom::Length.id()));
    }
    object.drop_with(&mut vm.session.heap);
    vm.new_array(names)
}

pub(super) fn from_entries(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let entries = vm.iterate_to_vec(arg(args, 0).clone_with_heap(&vm.session.heap))?;
    let object = plain_object(vm);
    let result = JsValue::Ref(object);
    for entry in entries {
        if !entry.is_object() {
            let shown = vm.inspect_short(&entry)?;
            entry.drop_with(&mut vm.session.heap);
            result.drop_with(&mut vm.session.heap);
            return Err(vm.type_error(&format!("Iterator value {shown} is not an entry object")));
        }
        let key = vm.get(&entry, PropertyKey::Index(0))?;
        let value = vm.get(&entry, PropertyKey::Index(1))?;
        entry.drop_with(&mut vm.session.heap);
        let key = vm.coerce_key(key)?;
        vm.define_data(object, key, value);
    }
    Ok(result)
}

pub(super) fn has_own(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let object = object_arg(vm, args, "hasOwn")?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let key = vm.coerce_key(arg(args, 1).clone_with_heap(&vm.session.heap))?;
    let found = vm.has_own_id(*id, key);
    object.drop_with(&mut vm.session.heap);
    Ok(JsValue::Bool(found))
}

pub(super) fn is(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    Ok(JsValue::Bool(vm.same_value(arg(args, 0), arg(args, 1))))
}

pub(super) fn proto_has_own_property(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let key = vm.coerce_key(arg(args, 0).clone_with_heap(&vm.session.heap))?;
    let object = to_object(vm, this)?;
    let JsValue::Ref(id) = &object else { unreachable!() };
    let found = vm.has_own_id(*id, key);
    object.drop_with(&mut vm.session.heap);
    Ok(JsValue::Bool(found))
}

/// `Object.prototype.toString`: `[object Tag]`.
pub(super) fn proto_to_string(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let tag = match this {
        JsValue::Undefined | JsValue::Hole => "Undefined".to_owned(),
        JsValue::Null => "Null".to_owned(),
        JsValue::Ref(id) => {
            let custom = vm.get(this, PropertyKey::Symbol(WellKnownSymbol::ToStringTag.id()))?;
            let custom_text = vm.string_of(&custom);
            custom.drop_with(&mut vm.session.heap);
            match custom_text {
                Some(text) => text.to_string_lossy(),
                None => match vm.object(*id).map(|o| &o.class) {
                    Some(ObjectClass::Array) => "Array".to_owned(),
                    Some(ObjectClass::Function(_)) => "Function".to_owned(),
                    Some(ObjectClass::Error) => "Error".to_owned(),
                    Some(ObjectClass::Arguments) => "Arguments".to_owned(),
                    Some(ObjectClass::BoxedBool(_)) => "Boolean".to_owned(),
                    Some(ObjectClass::BoxedNumber(_)) => "Number".to_owned(),
                    Some(ObjectClass::BoxedString(_)) => "String".to_owned(),
                    Some(ObjectClass::RegExp(_)) => "RegExp".to_owned(),
                    _ => "Object".to_owned(),
                },
            }
        }
        JsValue::Bool(_) => "Boolean".to_owned(),
        JsValue::Number(_) => "Number".to_owned(),
        JsValue::Atom(_) | JsValue::Str(_) => "String".to_owned(),
        JsValue::Symbol(_) => "Symbol".to_owned(),
        JsValue::BigInt(_) => "BigInt".to_owned(),
    };
    vm.alloc_string(JsString::from(format!("[object {tag}]").as_str()))
}

pub(super) fn proto_value_of(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    to_object(vm, this)
}

pub(super) fn proto_is_prototype_of(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let this_id = this_object(vm, this, "Object.prototype.isPrototypeOf")?;
    let JsValue::Ref(start) = arg(args, 0) else {
        return Ok(JsValue::Bool(false));
    };
    let mut current = *start;
    while let Some(proto) = vm.get_prototype(current) {
        if proto == this_id {
            return Ok(JsValue::Bool(true));
        }
        current = proto;
    }
    Ok(JsValue::Bool(false))
}

pub(super) fn proto_property_is_enumerable(
    vm: &mut Vm<'_>,
    this: &JsValue,
    args: &[JsValue],
    _: bool,
) -> RunResult<JsValue> {
    let key = vm.coerce_key(arg(args, 0).clone_with_heap(&vm.session.heap))?;
    let JsValue::Ref(id) = this else {
        return Ok(JsValue::Bool(false));
    };
    let enumerable = vm.object(*id).is_some_and(|object| match object.props.get(&key) {
        Some(slot) => slot.flags().enumerable(),
        None => vm.has_own(object, key) && !matches!(key, PropertyKey::Atom(a) if a == StaticAtom::Length.id()),
    });
    Ok(JsValue::Bool(enumerable))
}
