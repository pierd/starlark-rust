/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Possible optimisations:
// Preallocate int, none, bool etc slots in Value, so they are shared
// Encoding none, bool etc in the pointer of frozen value

use crate::values::{
    layout::{
        arena::Arena,
        arena2::{AValuePtr, Arena2, Reservation},
        avalue::{complex, simple, AValue},
        pointer::Pointer,
        value::{FrozenValue, Value, ValueMem},
    },
    AllocFrozenValue, ComplexValue, SimpleValue,
};
use gazebo::{cast, prelude::*};
use std::{
    cell::RefCell,
    collections::HashSet,
    fmt,
    fmt::{Debug, Formatter},
    hash::{Hash, Hasher},
    mem,
    ops::Deref,
    ptr,
    sync::Arc,
};

/// A heap on which [`Value`]s can be allocated. The values will be annotated with the heap lifetime.
#[derive(Default)]
pub struct Heap {
    // Should really be ValueMem<'v>, where &'v self
    arena: RefCell<Arena<ValueMem<'static>>>,
}

impl Debug for Heap {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut x = f.debug_struct("Heap");
        x.field(
            "bytes",
            &self.arena().try_borrow().map(|x| x.allocated_bytes()),
        );
        x.finish()
    }
}

/// A heap on which [`FrozenValue`]s can be allocated.
/// Can be kept alive by a [`FrozenHeapRef`].
#[derive(Default)]
pub struct FrozenHeap {
    arena: Arena2,                         // My memory
    refs: RefCell<HashSet<FrozenHeapRef>>, // Memory I depend on
}

impl Debug for FrozenHeap {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut x = f.debug_struct("FrozenHeap");
        x.field("bytes", &self.arena.allocated_bytes());
        x.field("refs", &self.refs.try_borrow().map(|x| x.len()));
        x.finish()
    }
}

// Safe because we never mutate the Arena other than with &mut
unsafe impl Sync for FrozenHeapRef {}
unsafe impl Send for FrozenHeapRef {}

/// A reference to a [`FrozenHeap`] that keeps alive all values on the underlying heap.
/// Note that the [`Hash`] is consistent for a single [`FrozenHeapRef`], but non-deterministic
/// across executions and distinct but observably identical [`FrozenHeapRef`] values.
#[derive(Clone, Dupe, Debug)]
// The Eq/Hash are by pointer rather than value, since we produce unique values
// given an underlying FrozenHeap.
pub struct FrozenHeapRef(Arc<FrozenHeap>);

impl Hash for FrozenHeapRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let x: &FrozenHeap = Deref::deref(&self.0);
        ptr::hash(x, state);
    }
}

impl PartialEq<FrozenHeapRef> for FrozenHeapRef {
    fn eq(&self, other: &FrozenHeapRef) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for FrozenHeapRef {}

impl FrozenHeap {
    /// Create a new [`FrozenHeap`].
    pub fn new() -> Self {
        Self::default()
    }

    /// After all values have been allocated, convert the [`FrozenHeap`] into a
    /// [`FrozenHeapRef`] which can be [`clone`](Clone::clone)d, shared between threads,
    /// and ensures the underlying values allocated on the [`FrozenHeap`] remain valid.
    pub fn into_ref(self) -> FrozenHeapRef {
        FrozenHeapRef(Arc::new(self))
    }

    /// Keep the argument [`FrozenHeapRef`] alive as long as this [`FrozenHeap`]
    /// is kept alive. Used if a [`FrozenValue`] in this heap points at values in another
    /// [`FrozenHeap`].
    pub fn add_reference(&self, heap: &FrozenHeapRef) {
        self.refs.borrow_mut().get_or_insert_owned(heap);
    }

    fn alloc_raw(&self, x: impl AValue<'static>) -> FrozenValue {
        let v: &AValuePtr = self.arena.alloc(x);
        FrozenValue(Pointer::new_ptr1(unsafe { cast::ptr_lifetime(v) }))
    }

    pub(crate) fn alloc_str(&self, x: Box<str>) -> FrozenValue {
        self.alloc_raw(simple(x))
    }

    /// Allocate a [`SimpleValue`] on this heap. Be careful about the warnings
    /// around [`FrozenValue`].
    pub fn alloc_simple(&self, val: impl SimpleValue) -> FrozenValue {
        self.alloc_raw(simple(val))
    }
}

/// Used to `freeze` values by [`ComplexValue::freeze`].
// A freezer is a pair of the FrozenHeap and a "magic" value,
// which we happen to use for the slots (see `FrozenSlotsRef`)
// but could be used for anything.
pub struct Freezer(FrozenHeap, FrozenValue, Option<Reservation<'static>>);

impl Freezer {
    pub(crate) fn new<T: SimpleValue>(heap: FrozenHeap) -> Self {
        fn reserve<'v, 'v2, T>(heap: &'v FrozenHeap, _ty: Option<T>) -> Reservation<'v>
        where
            'v2: 'v,
            T: AValue<'v2>,
        {
            heap.arena.reserve::<T>()
        }

        // Slightly odd construction as we want to produce a reservation with type
        // simple(T), but simple is a function with an opaque impl return, so fake up
        // an empty Option containing the right type.
        let ty: Option<T> = None;
        let ty = ty.map(simple);
        let reservation = reserve(&heap, ty);
        // Morally the type is tied to the heap, but we want to have them both next to each other
        let reservation = unsafe { transmute!(Reservation, Reservation<'static>, reservation) };
        let fv = FrozenValue(Pointer::new_ptr1(unsafe {
            cast::ptr_lifetime(reservation.ptr())
        }));
        Self(heap, fv, Some(reservation))
    }

    pub(crate) fn set_magic(&mut self, val: impl SimpleValue) {
        let reservation = self.2.take().expect("Can only call set_magic once");
        reservation.fill(simple(val))
    }

    pub(crate) fn get_magic(&self) -> FrozenValue {
        self.1
    }

    pub(crate) fn into_ref(self) -> FrozenHeapRef {
        self.0.into_ref()
    }

    /// Allocate a new value while freezing. Usually not a great idea.
    pub fn alloc<'v, T: AllocFrozenValue>(&'v self, val: T) -> FrozenValue {
        val.alloc_frozen_value(&self.0)
    }

    pub(crate) fn reserve<'v, 'v2, T: AValue<'v2>>(&'v self) -> Reservation<'v>
    where
        'v2: 'v,
    {
        self.0.arena.reserve::<T>()
    }

    /// Freeze a nested value while freezing yourself.
    pub fn freeze(&self, value: Value) -> anyhow::Result<FrozenValue> {
        // Case 1: We have our value encoded in our pointer
        if let Some(x) = value.unpack_frozen() {
            return Ok(x);
        }

        // Case 2: We have already been replaced with a Forward node
        let value = value.0.unpack_ptr2().unwrap();
        match value {
            ValueMem::Forward(x) => return Ok(*x),
            _ => {}
        }

        // Case 3: We need to be moved to the new heap
        // Invariant: After this method completes ValueMem must be of type Forward
        let reservation = value.get_ref().reserve_simple(self);
        let value_mut = value as *const ValueMem as *mut ValueMem;
        let fv = FrozenValue(Pointer::new_ptr1(unsafe {
            cast::ptr_lifetime(reservation.0.ptr())
        }));
        // Important we allocate the location for the frozen value _before_ we copy it
        // so that cycles still work
        let v = unsafe { ptr::replace(value_mut, ValueMem::Forward(fv)) };

        match v {
            ValueMem::Str(x) => reservation.0.fill(simple(x)),
            ValueMem::AValue(x) => x.fill_simple(reservation, self)?,
            _ => {
                // We don't expect Unitialized, because that is not a real value.
                // We don't expect Forward since that is handled in step 2.
                // We don't expect Copied or Blackhole because that only happens during GC.
                // We don't expect CallEnter/CallExit as that is only during profiling on the heap, and not referenced.
                // We don't expect Ref, because that only occurs inside ValueRef, and that has a custom freeze.
                v.unexpected("FrozenHeap::freeze case 3")
            }
        }
        Ok(fv)
    }
}

impl Heap {
    /// Create a new [`Heap`].
    pub fn new() -> Self {
        Self::default()
    }

    fn arena<'v>(&'v self) -> &'v RefCell<Arena<ValueMem<'v>>> {
        // Not totally safe because variance on &self might mean we create ValueMem
        // at a smaller lifetime than it has to be. But approximately correct.
        unsafe {
            transmute!(
                &'v RefCell<Arena<ValueMem<'static>>>,
                &'v RefCell<Arena<ValueMem<'v>>>,
                &self.arena
            )
        }
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.arena().borrow().allocated_bytes()
    }

    pub(crate) fn alloc_raw<'v>(&'v self, v: ValueMem<'v>) -> Value<'v> {
        let arena_ref = self.arena().borrow_mut();
        let arena = &*arena_ref;

        // We have an arena inside a RefCell which stores ValueMem<'v>
        // However, we promise not to clear the RefCell other than for GC
        // so we can make the `arena` available longer
        let arena = unsafe { transmute!(&Arena<ValueMem<'v>>, &'v Arena<ValueMem<'v>>, arena) };

        Value(Pointer::new_ptr2(arena.alloc(v)))
    }

    pub(crate) fn alloc_str(&self, x: Box<str>) -> Value {
        self.alloc_raw(ValueMem::Str(x))
    }

    /// Allocate a [`SimpleValue`] on the [`Heap`].
    pub fn alloc_simple<'v>(&'v self, x: impl SimpleValue) -> Value<'v> {
        let x: Box<dyn AValue<'static>> = box simple(x);
        // Safe because we know we can restrict an AValue
        let x: Box<dyn AValue<'v>> = unsafe { mem::transmute(x) };
        self.alloc_raw(ValueMem::AValue(x))
    }

    /// Allocate a [`ComplexValue`] on the [`Heap`].
    pub fn alloc_complex<'v>(&'v self, x: impl ComplexValue<'v>) -> Value<'v> {
        self.alloc_raw(ValueMem::AValue(box complex(x)))
    }

    pub(crate) fn for_each<'v>(&'v self, f: impl FnMut(&'v ValueMem<'v>)) {
        let mut arena_ref = self.arena().borrow_mut();
        let arena = &mut *arena_ref;

        // We have an arena inside a RefCell which stores ValueMem<'static>
        // However, we promise not to clear the RefCell other than for GC
        // and we promise the ValueMem's are only valid for 'v
        // so we cast the `arena` to how we actually know it works
        let arena =
            unsafe { transmute!(&mut Arena<ValueMem<'v>>, &'v mut Arena<ValueMem<'v>>, arena) };

        arena.for_each(f);
    }

    /// Garbage collect any values that are unused. This function is _unsafe_ in
    /// the sense that any `Value<'v>` not returned by `Tracer` _will become
    /// invalid_. Furthermore, any references to values, e.g `&'v str` will
    /// also become invalid.
    pub(crate) unsafe fn garbage_collect<'v>(&'v self, f: impl FnOnce(&Tracer<'v>)) {
        self.garbage_collect_internal(f)
    }

    fn garbage_collect_internal<'v>(&'v self, f: impl FnOnce(&Tracer<'v>)) {
        // Must rewrite all Value's so they point at the new heap
        let mut arena = self.arena().borrow_mut();

        let traceer = Tracer::<'v> {
            arena: Arena::new(),
        };
        f(&traceer);
        *arena = traceer.arena;
    }
}

/// Used to perform garbage collection by [`Trace::trace`](crate::values::Trace::trace).
pub struct Tracer<'v> {
    arena: Arena<ValueMem<'v>>,
}

impl<'v> Tracer<'v> {
    /// Walk over a value during garbage collection.
    pub fn trace(&self, value: &mut Value<'v>) {
        *value = self.adjust(*value)
    }

    fn adjust(&self, value: Value<'v>) -> Value<'v> {
        let old_val = value.0.unpack_ptr2();
        // Case 1, doesn't point at the old arena
        if old_val.is_none() {
            return value;
        }

        // Case 2: We have already been replaced with a Copied node
        let old_val = old_val.unwrap();
        if let ValueMem::Copied(v) = old_val {
            return *v;
        }

        // Case 3: We need to be moved to the new heap
        // Invariant: After this method completes ValueMem must be of type Copied
        let old_mem = old_val as *const ValueMem<'v> as *mut ValueMem<'v>;
        // We know the arena we are allocating into will live for 'v
        let new_mem = unsafe {
            transmute!(
                &mut ValueMem<'v>,
                &'v mut ValueMem<'v>,
                self.arena.alloc(ValueMem::Blackhole)
            )
        };
        let mut new_val: Value<'v> = Value(Pointer::new_ptr2(new_mem));
        if value.0.get_user_tag() {
            // SUPER IMPORTANT:
            // There are invariants around user tags (whether something is a Ref), so make sure they get copied over.
            new_val = Value(new_val.0.set_user_tag());
        }

        let mut old_mem = unsafe { ptr::replace(old_mem, ValueMem::Copied(new_val)) };

        match &mut old_mem {
            ValueMem::AValue(x) => x.trace(self),
            _ => {} // Doesn't contain Value pointers
        }
        unsafe {
            ptr::replace(new_mem as *const ValueMem<'v> as *mut ValueMem<'v>, old_mem)
        };
        new_val
    }
}

#[test]
fn test_send_sync()
where
    FrozenHeapRef: Send + Sync,
{
}
