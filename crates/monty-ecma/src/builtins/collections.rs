//! `Map`, `Set`, `WeakMap` and `WeakSet`, plus their iterators and `groupBy`.
//!
//! The weak collections hold their keys strongly: the sandbox has no
//! finalization, so a key stays alive for as long as the collection does.

use std::mem;

use super::{NativeFn, arg, install_constructor, install_function, install_getter, raw_object, require_callable};
use crate::{
    error::RunResult,
    heap::{HeapId, IterKind, MapData, MapEntry, ObjectClass, PropFlags, PropertyKey},
    intern::WellKnownSymbol,
    value::JsValue,
    vm::Vm,
};

/// Which collection a native belongs to, for its receiver checks and messages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Map,
    Set,
    WeakMap,
    WeakSet,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Map => "Map",
            Self::Set => "Set",
            Self::WeakMap => "WeakMap",
            Self::WeakSet => "WeakSet",
        }
    }

    fn matches(self, class: &ObjectClass) -> bool {
        matches!(
            (self, class),
            (Self::Map, ObjectClass::Map(_))
                | (Self::Set, ObjectClass::Set(_))
                | (Self::WeakMap, ObjectClass::WeakMap(_))
                | (Self::WeakSet, ObjectClass::WeakSet(_))
        )
    }
}

pub(super) fn install(vm: &mut Vm<'_>, global: HeapId) {
    let object_prototype = vm.session.realm.object_prototype;
    let iterator_prototype = vm.session.realm.iterator_prototype;
    // Map
    let map_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.map_prototype = map_prototype;
    for native in [
        NativeFn::MapProtoGet,
        NativeFn::MapProtoSet,
        NativeFn::MapProtoHas,
        NativeFn::MapProtoDelete,
        NativeFn::MapProtoClear,
        NativeFn::MapProtoForEach,
        NativeFn::MapProtoKeys,
        NativeFn::MapProtoValues,
    ] {
        install_function(vm, map_prototype, native);
    }
    let entries = install_function(vm, map_prototype, NativeFn::MapProtoEntries);
    vm.session.heap.inc_ref(entries);
    vm.define_with_flags(
        map_prototype,
        PropertyKey::Symbol(WellKnownSymbol::Iterator.id()),
        JsValue::Ref(entries),
        PropFlags::HIDDEN,
    );
    install_getter(vm, map_prototype, "size", NativeFn::MapProtoSize);
    install_tag(vm, map_prototype, "Map");
    let map = install_constructor(vm, global, NativeFn::Map, map_prototype);
    install_function(vm, map, NativeFn::MapGroupBy);
    // Set
    let set_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.set_prototype = set_prototype;
    for native in [
        NativeFn::SetProtoAdd,
        NativeFn::SetProtoHas,
        NativeFn::SetProtoDelete,
        NativeFn::SetProtoClear,
        NativeFn::SetProtoForEach,
        NativeFn::SetProtoEntries,
    ] {
        install_function(vm, set_prototype, native);
    }
    let values = install_function(vm, set_prototype, NativeFn::SetProtoValues);
    for key in [
        PropertyKey::Symbol(WellKnownSymbol::Iterator.id()),
        super::key(vm, "keys"),
    ] {
        vm.session.heap.inc_ref(values);
        vm.define_with_flags(set_prototype, key, JsValue::Ref(values), PropFlags::HIDDEN);
    }
    install_getter(vm, set_prototype, "size", NativeFn::SetProtoSize);
    install_tag(vm, set_prototype, "Set");
    install_constructor(vm, global, NativeFn::Set, set_prototype);
    // WeakMap and WeakSet
    let weak_map_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.weak_map_prototype = weak_map_prototype;
    for native in [
        NativeFn::WeakMapProtoGet,
        NativeFn::WeakMapProtoSet,
        NativeFn::WeakMapProtoHas,
        NativeFn::WeakMapProtoDelete,
    ] {
        install_function(vm, weak_map_prototype, native);
    }
    install_tag(vm, weak_map_prototype, "WeakMap");
    install_constructor(vm, global, NativeFn::WeakMap, weak_map_prototype);
    let weak_set_prototype = raw_object(vm, Some(object_prototype));
    vm.session.realm.weak_set_prototype = weak_set_prototype;
    for native in [
        NativeFn::WeakSetProtoAdd,
        NativeFn::WeakSetProtoHas,
        NativeFn::WeakSetProtoDelete,
    ] {
        install_function(vm, weak_set_prototype, native);
    }
    install_tag(vm, weak_set_prototype, "WeakSet");
    install_constructor(vm, global, NativeFn::WeakSet, weak_set_prototype);
    // iterators
    let map_iterator_prototype = raw_object(vm, Some(iterator_prototype));
    vm.session.realm.map_iterator_prototype = map_iterator_prototype;
    install_function(vm, map_iterator_prototype, NativeFn::ArrayIteratorNext);
    install_tag(vm, map_iterator_prototype, "Map Iterator");
    let set_iterator_prototype = raw_object(vm, Some(iterator_prototype));
    vm.session.realm.set_iterator_prototype = set_iterator_prototype;
    install_function(vm, set_iterator_prototype, NativeFn::ArrayIteratorNext);
    install_tag(vm, set_iterator_prototype, "Set Iterator");
}

/// Defines `Symbol.toStringTag` on a prototype.
fn install_tag(vm: &mut Vm<'_>, prototype: HeapId, tag: &str) {
    let tag = vm.atom_value(tag);
    vm.define_with_flags(
        prototype,
        PropertyKey::Symbol(WellKnownSymbol::ToStringTag.id()),
        tag,
        PropFlags::CONFIGURABLE,
    );
}

/// The collection a method was called on, or V8's incompatible-receiver error.
fn receiver(vm: &mut Vm<'_>, this: &JsValue, kind: Kind, method: &str) -> RunResult<HeapId> {
    if let JsValue::Ref(id) = this
        && vm.object(*id).is_some_and(|o| kind.matches(&o.class))
    {
        return Ok(*id);
    }
    let shown = vm.inspect_short(this)?;
    Err(vm.type_error(&format!(
        "Method {}.prototype.{method} called on incompatible receiver {shown}",
        kind.name()
    )))
}

/// Mutable access to a collection's entries.
fn data_mut<'a>(vm: &'a mut Vm<'_>, id: HeapId) -> &'a mut MapData {
    match vm.object_mut(id).map(|o| &mut o.class) {
        Some(
            ObjectClass::Map(data) | ObjectClass::Set(data) | ObjectClass::WeakMap(data) | ObjectClass::WeakSet(data),
        ) => data,
        _ => unreachable!("checked by `receiver`"),
    }
}

fn data<'a>(vm: &'a Vm<'_>, id: HeapId) -> &'a MapData {
    match vm.object(id).map(|o| &o.class) {
        Some(
            ObjectClass::Map(data) | ObjectClass::Set(data) | ObjectClass::WeakMap(data) | ObjectClass::WeakSet(data),
        ) => data,
        _ => unreachable!("checked by `receiver`"),
    }
}

/// `new Map(iterable?)` / `new Set(iterable?)` / the weak variants.
fn construct(vm: &mut Vm<'_>, kind: Kind, args: &[JsValue], construct: bool) -> RunResult<JsValue> {
    if !construct {
        return Err(vm.type_error(&format!("Constructor {} requires 'new'", kind.name())));
    }
    let (proto, class) = match kind {
        Kind::Map => (vm.session.realm.map_prototype, ObjectClass::Map(Box::default())),
        Kind::Set => (vm.session.realm.set_prototype, ObjectClass::Set(Box::default())),
        Kind::WeakMap => (
            vm.session.realm.weak_map_prototype,
            ObjectClass::WeakMap(Box::default()),
        ),
        Kind::WeakSet => (
            vm.session.realm.weak_set_prototype,
            ObjectClass::WeakSet(Box::default()),
        ),
    };
    let id = vm.new_class_object(proto, class);
    let collection = JsValue::Ref(id);
    let source = arg(args, 0);
    if source.is_nullish() {
        return Ok(collection);
    }
    let items = match vm.iterate_to_vec(source.clone_with_heap(&vm.session.heap)) {
        Ok(items) => items,
        Err(err) => {
            collection.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    };
    for item in items {
        let outcome = match kind {
            Kind::Map | Kind::WeakMap => insert_entry(vm, id, kind, &item),
            Kind::Set | Kind::WeakSet => insert_key(vm, id, kind, &item),
        };
        item.drop_with(&mut vm.session.heap);
        if let Err(err) = outcome {
            collection.drop_with(&mut vm.session.heap);
            return Err(err);
        }
    }
    Ok(collection)
}

/// Adds a `[key, value]` entry object to a map.
fn insert_entry(vm: &mut Vm<'_>, map: HeapId, kind: Kind, entry: &JsValue) -> RunResult<()> {
    if !entry.is_object() {
        let shown = vm.inspect_short(entry)?;
        return Err(vm.type_error(&format!("Iterator value {shown} is not an entry object")));
    }
    let key = vm.get(entry, PropertyKey::Index(0))?;
    let value = vm.get(entry, PropertyKey::Index(1))?;
    set_value(vm, map, kind, key, value)
}

/// Adds a key (with an `undefined` value) to a set.
fn insert_key(vm: &mut Vm<'_>, set: HeapId, kind: Kind, key: &JsValue) -> RunResult<()> {
    set_value(vm, set, kind, key.clone_with_heap(&vm.session.heap), JsValue::Undefined)
}

/// Stores `key => value`, consuming both; a weak collection rejects
/// non-object keys.
fn set_value(vm: &mut Vm<'_>, id: HeapId, kind: Kind, key: JsValue, value: JsValue) -> RunResult<()> {
    if matches!(kind, Kind::WeakMap | Kind::WeakSet) && !key.is_object() {
        vm.drop_all(key, vec![value]);
        let message = if kind == Kind::WeakMap {
            "Invalid value used as weak map key"
        } else {
            "Invalid value used in weak set"
        };
        return Err(vm.type_error(message));
    }
    // a `-0` key is stored as `+0`
    let key = match key {
        JsValue::Number(n) if n == 0.0 && n.is_sign_negative() => JsValue::Number(0.0),
        other => other,
    };
    let hash = vm.map_key(&key);
    let existing = data(vm, id).index.get(&hash).copied();
    if let Some(index) = existing {
        let entry = data_mut(vm, id).entries[index]
            .as_mut()
            .expect("indexed entries are live");
        let old_key = mem::replace(&mut entry.key, key);
        let old_value = mem::replace(&mut entry.value, value);
        vm.drop_all(old_key, vec![old_value]);
    } else {
        let (len, capacity) = {
            let data = data(vm, id);
            (data.entries.len(), data.entries.capacity())
        };
        if let Err(err) = vm.tracker().check_growth(len, capacity, 64) {
            vm.drop_all(key, vec![value]);
            return Err(err.into());
        }
        let data = data_mut(vm, id);
        data.index.insert(hash.clone(), len);
        data.entries.push(Some(MapEntry { key, value, hash }));
        data.live += 1;
    }
    Ok(())
}

/// Looks a key up: the entry's index.
fn find(vm: &mut Vm<'_>, id: HeapId, key: &JsValue) -> Option<usize> {
    let hash = vm.map_key(key);
    data(vm, id).index.get(&hash).copied()
}

/// Removes a key, leaving a tombstone so live iterators keep their place.
fn remove(vm: &mut Vm<'_>, id: HeapId, key: &JsValue) -> bool {
    let hash = vm.map_key(key);
    let data = data_mut(vm, id);
    let Some(index) = data.index.remove(&hash) else {
        return false;
    };
    let entry = data.entries[index].take().expect("indexed entries are live");
    data.live -= 1;
    vm.drop_all(entry.key, vec![entry.value]);
    true
}

fn clear(vm: &mut Vm<'_>, id: HeapId) {
    let data = data_mut(vm, id);
    data.index.clear();
    data.live = 0;
    let entries: Vec<MapEntry> = data.entries.iter_mut().filter_map(Option::take).collect();
    for entry in entries {
        vm.drop_all(entry.key, vec![entry.value]);
    }
}

/// Snapshots the live entries for `forEach`, which may mutate the collection.
fn live_entries(vm: &Vm<'_>, id: HeapId) -> Vec<(JsValue, JsValue)> {
    data(vm, id)
        .entries
        .iter()
        .flatten()
        .map(|entry| {
            (
                entry.key.clone_with_heap(&vm.session.heap),
                entry.value.clone_with_heap(&vm.session.heap),
            )
        })
        .collect()
}

/// An iterator object over a collection.
fn iterator(vm: &mut Vm<'_>, id: HeapId, kind: Kind, iter_kind: IterKind) -> JsValue {
    let proto = if kind == Kind::Set {
        vm.session.realm.set_iterator_prototype
    } else {
        vm.session.realm.map_iterator_prototype
    };
    vm.session.heap.inc_ref(id);
    let object = vm.new_class_object(
        proto,
        ObjectClass::MapIterator {
            map: id,
            index: 0,
            kind: iter_kind,
        },
    );
    JsValue::Ref(object)
}

// === Map ===

pub(super) fn map(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct_call: bool) -> RunResult<JsValue> {
    construct(vm, Kind::Map, args, construct_call)
}

pub(super) fn map_get(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "get")?;
    Ok(match find(vm, id, arg(args, 0)) {
        Some(index) => data(vm, id).entries[index]
            .as_ref()
            .map_or(JsValue::Undefined, |entry| {
                entry.value.clone_with_heap(&vm.session.heap)
            }),
        None => JsValue::Undefined,
    })
}

pub(super) fn map_set(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "set")?;
    let key = arg(args, 0).clone_with_heap(&vm.session.heap);
    let value = arg(args, 1).clone_with_heap(&vm.session.heap);
    set_value(vm, id, Kind::Map, key, value)?;
    Ok(this.clone_with_heap(&vm.session.heap))
}

pub(super) fn map_has(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "has")?;
    Ok(JsValue::Bool(find(vm, id, arg(args, 0)).is_some()))
}

pub(super) fn map_delete(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "delete")?;
    Ok(JsValue::Bool(remove(vm, id, arg(args, 0))))
}

pub(super) fn map_clear(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "clear")?;
    clear(vm, id);
    Ok(JsValue::Undefined)
}

pub(super) fn map_size(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "size")?;
    Ok(JsValue::Number(data(vm, id).live as f64))
}

pub(super) fn map_for_each(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "forEach")?;
    for_each(vm, this, id, args)
}

/// `forEach(callback(value, key, collection), thisArg)` over a live snapshot.
fn for_each(vm: &mut Vm<'_>, this: &JsValue, id: HeapId, args: &[JsValue]) -> RunResult<JsValue> {
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    for (key, value) in live_entries(vm, id) {
        let result = vm.call(
            callback.clone_with_heap(&vm.session.heap),
            this_arg.clone_with_heap(&vm.session.heap),
            vec![value, key, this.clone_with_heap(&vm.session.heap)],
        )?;
        result.drop_with(&mut vm.session.heap);
    }
    Ok(JsValue::Undefined)
}

pub(super) fn map_keys(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "keys")?;
    Ok(iterator(vm, id, Kind::Map, IterKind::Keys))
}

pub(super) fn map_values(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "values")?;
    Ok(iterator(vm, id, Kind::Map, IterKind::Values))
}

pub(super) fn map_entries(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Map, "entries")?;
    Ok(iterator(vm, id, Kind::Map, IterKind::Entries))
}

/// `Map.groupBy(items, callback)`.
pub(super) fn map_group_by(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let callback = arg(args, 1);
    require_callable(vm, callback)?;
    let items = vm.iterate_to_vec(arg(args, 0).clone_with_heap(&vm.session.heap))?;
    let proto = vm.session.realm.map_prototype;
    let map = vm.new_class_object(proto, ObjectClass::Map(Box::default()));
    let result = JsValue::Ref(map);
    for (index, item) in items.into_iter().enumerate() {
        let group = vm.call(
            callback.clone_with_heap(&vm.session.heap),
            JsValue::Undefined,
            vec![item.clone_with_heap(&vm.session.heap), JsValue::Number(index as f64)],
        );
        let group = match group {
            Ok(group) => group,
            Err(err) => {
                item.drop_with(&mut vm.session.heap);
                result.drop_with(&mut vm.session.heap);
                return Err(err);
            }
        };
        let bucket = if let Some(entry) = find(vm, map, &group) {
            data(vm, map).entries[entry]
                .as_ref()
                .map(|entry| entry.value.clone_with_heap(&vm.session.heap))
                .expect("live")
        } else {
            let bucket = vm.new_array(Vec::new())?;
            set_value(vm, map, Kind::Map, group, bucket.clone_with_heap(&vm.session.heap))?;
            bucket
        };
        if let JsValue::Ref(array) = &bucket {
            vm.array_push(*array, item)?;
        }
        bucket.drop_with(&mut vm.session.heap);
    }
    Ok(result)
}

/// `Object.groupBy(items, callback)`: a null-prototype object of arrays.
pub(super) fn object_group_by(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let callback = arg(args, 1);
    require_callable(vm, callback)?;
    let items = vm.iterate_to_vec(arg(args, 0).clone_with_heap(&vm.session.heap))?;
    let result = vm.new_object_with_proto(None);
    let JsValue::Ref(object) = &result else { unreachable!() };
    let object = *object;
    for (index, item) in items.into_iter().enumerate() {
        let group = vm.call(
            callback.clone_with_heap(&vm.session.heap),
            JsValue::Undefined,
            vec![item.clone_with_heap(&vm.session.heap), JsValue::Number(index as f64)],
        );
        let key = match group.and_then(|group| vm.coerce_key(group)) {
            Ok(key) => key,
            Err(err) => {
                item.drop_with(&mut vm.session.heap);
                result.drop_with(&mut vm.session.heap);
                return Err(err);
            }
        };
        let bucket = if vm.has_own_id(object, key) {
            vm.get(&result, key)?
        } else {
            let bucket = vm.new_array(Vec::new())?;
            vm.define_data(object, key, bucket.clone_with_heap(&vm.session.heap));
            bucket
        };
        if let JsValue::Ref(array) = &bucket {
            vm.array_push(*array, item)?;
        }
        bucket.drop_with(&mut vm.session.heap);
    }
    Ok(result)
}

// === Set ===

pub(super) fn set(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct_call: bool) -> RunResult<JsValue> {
    construct(vm, Kind::Set, args, construct_call)
}

pub(super) fn set_add(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "add")?;
    insert_key(vm, id, Kind::Set, arg(args, 0))?;
    Ok(this.clone_with_heap(&vm.session.heap))
}

pub(super) fn set_has(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "has")?;
    Ok(JsValue::Bool(find(vm, id, arg(args, 0)).is_some()))
}

pub(super) fn set_delete(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "delete")?;
    Ok(JsValue::Bool(remove(vm, id, arg(args, 0))))
}

pub(super) fn set_clear(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "clear")?;
    clear(vm, id);
    Ok(JsValue::Undefined)
}

pub(super) fn set_size(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "size")?;
    Ok(JsValue::Number(data(vm, id).live as f64))
}

pub(super) fn set_for_each(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "forEach")?;
    // a set passes its value as both arguments
    let callback = arg(args, 0);
    require_callable(vm, callback)?;
    let this_arg = arg(args, 1);
    for (key, value) in live_entries(vm, id) {
        value.drop_with(&mut vm.session.heap);
        let result = vm.call(
            callback.clone_with_heap(&vm.session.heap),
            this_arg.clone_with_heap(&vm.session.heap),
            vec![
                key.clone_with_heap(&vm.session.heap),
                key,
                this.clone_with_heap(&vm.session.heap),
            ],
        )?;
        result.drop_with(&mut vm.session.heap);
    }
    Ok(JsValue::Undefined)
}

pub(super) fn set_values(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "values")?;
    Ok(iterator(vm, id, Kind::Set, IterKind::Keys))
}

pub(super) fn set_entries(vm: &mut Vm<'_>, this: &JsValue, _: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::Set, "entries")?;
    Ok(iterator(vm, id, Kind::Set, IterKind::Entries))
}

// === WeakMap and WeakSet ===

pub(super) fn weak_map(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct_call: bool) -> RunResult<JsValue> {
    construct(vm, Kind::WeakMap, args, construct_call)
}

pub(super) fn weak_map_get(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakMap, "get")?;
    Ok(match find(vm, id, arg(args, 0)) {
        Some(index) => data(vm, id).entries[index]
            .as_ref()
            .map_or(JsValue::Undefined, |entry| {
                entry.value.clone_with_heap(&vm.session.heap)
            }),
        None => JsValue::Undefined,
    })
}

pub(super) fn weak_map_set(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakMap, "set")?;
    let key = arg(args, 0).clone_with_heap(&vm.session.heap);
    let value = arg(args, 1).clone_with_heap(&vm.session.heap);
    set_value(vm, id, Kind::WeakMap, key, value)?;
    Ok(this.clone_with_heap(&vm.session.heap))
}

pub(super) fn weak_map_has(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakMap, "has")?;
    Ok(JsValue::Bool(find(vm, id, arg(args, 0)).is_some()))
}

pub(super) fn weak_map_delete(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakMap, "delete")?;
    Ok(JsValue::Bool(remove(vm, id, arg(args, 0))))
}

pub(super) fn weak_set(vm: &mut Vm<'_>, _: &JsValue, args: &[JsValue], construct_call: bool) -> RunResult<JsValue> {
    construct(vm, Kind::WeakSet, args, construct_call)
}

pub(super) fn weak_set_add(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakSet, "add")?;
    insert_key(vm, id, Kind::WeakSet, arg(args, 0))?;
    Ok(this.clone_with_heap(&vm.session.heap))
}

pub(super) fn weak_set_has(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakSet, "has")?;
    Ok(JsValue::Bool(find(vm, id, arg(args, 0)).is_some()))
}

pub(super) fn weak_set_delete(vm: &mut Vm<'_>, this: &JsValue, args: &[JsValue], _: bool) -> RunResult<JsValue> {
    let id = receiver(vm, this, Kind::WeakSet, "delete")?;
    Ok(JsValue::Bool(remove(vm, id, arg(args, 0))))
}
