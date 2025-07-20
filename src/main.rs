#![feature(box_vec_non_null)]
#![feature(unsized_fn_params)]
#![feature(ptr_metadata)]
#![feature(forget_unsized)]
#![feature(layout_for_ptr)]
#![feature(vec_into_raw_parts)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

// TODO: Currently JUST for 64-bit

use core::{cmp::PartialEq, fmt::Debug, ops::Deref, ptr::NonNull, sync::atomic::AtomicUsize};
use std::{
    alloc::{Layout, alloc, dealloc},
    any::Any,
    marker::PhantomData,
    mem::{self, ManuallyDrop, size_of, size_of_val_raw},
    ptr::{self, Pointee, copy_nonoverlapping, drop_in_place},
    slice,
};

// Lightweight arc
struct Arc<T: ?Sized>(NonNull<()>, PhantomData<T>);

impl<T: ?Sized> Arc<T> {
    const fn layout_dataless() -> (usize, Layout) {
        let layout = Layout::new::<AtomicUsize>();
        if let Ok((layout, metadata_offset)) =
            layout.extend(Layout::new::<<T as Pointee>::Metadata>())
        {
            return (metadata_offset, layout);
        }
        unreachable!()
    }

    const fn layout_for(val: &T) -> (usize, usize, Layout) {
        let (metadata_offset, layout) = Self::layout_dataless();
        if let Ok((layout, data_offset)) = layout.extend(Layout::for_value(val)) {
            return (metadata_offset, data_offset, layout.pad_to_align());
        }
        unreachable!()
    }

    fn layout(&self) -> (*const T, Layout) {
        let (metadata_offset, layout) = Self::layout_dataless();
        unsafe {
            let metadata = self
                .0
                .byte_add(metadata_offset)
                .cast::<<T as Pointee>::Metadata>()
                .read();
            let dangling = ptr::from_raw_parts::<T>(ptr::dangling::<()>(), metadata); // Horrible
            let data_layout = Layout::from_size_align(
                mem::size_of_val_raw(dangling),
                mem::align_of_val_raw(dangling),
            )
            .unwrap();
            let (layout, data_offset) = layout.extend(data_layout).unwrap();
            (
                ptr::from_raw_parts(self.0.byte_add(data_offset).as_ptr(), metadata),
                layout.pad_to_align(),
            )
        }
    }

    fn new(val: Box<T>) -> Self {
        let (metadata_offset, data_offset, layout) = Self::layout_for(&*val);
        unsafe {
            let ptr = alloc(layout);

            ptr.cast::<AtomicUsize>().write(AtomicUsize::new(1));

            let val_ptr = Box::into_raw(val);
            let val_meta = val_ptr.to_raw_parts().1;

            ptr.byte_add(metadata_offset)
                .cast::<<T as Pointee>::Metadata>()
                .write(val_meta);

            copy_nonoverlapping(
                val_ptr.cast::<u8>(),
                ptr.byte_add(data_offset).cast::<u8>(),
                size_of_val_raw(val_ptr),
            );

            let data_layout = Layout::for_value_raw(val_ptr);
            dealloc(val_ptr.cast::<u8>(), data_layout);

            Arc(NonNull::new(ptr.cast()).unwrap(), PhantomData)
        }
    }
}

impl<T: ?Sized> Deref for Arc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.layout().0 }
    }
}

impl<T: ?Sized> Clone for Arc<T> {
    fn clone(&self) -> Self {
        unsafe {
            (*self.0.as_ptr().cast::<AtomicUsize>())
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        Arc(self.0, PhantomData)
    }
}

impl<T: ?Sized> Drop for Arc<T> {
    fn drop(&mut self) {
        unsafe {
            if (*self.0.as_ptr().cast::<AtomicUsize>())
                .fetch_sub(1, core::sync::atomic::Ordering::Relaxed)
                == 1
            {
                let (data, layout) = self.layout();
                drop_in_place(data.cast_mut());
                dealloc(self.0.as_ptr().cast(), layout);
            }
        }
    }
}

// unsafe impl<T> Send for Arc<T> {}
// unsafe impl<T> Sync for Arc<T> {}

impl<T: ?Sized + Debug> Debug for Arc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: ?Sized + PartialEq> PartialEq for Arc<T> {
    fn eq(&self, other: &Self) -> bool {
        (**self).eq(&**other)
    }
}

impl<T: ?Sized + Eq> Eq for Arc<T> {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Bits {
    Bits8,
    Bits16,
    Bits32,
    Bits64,
    BitsInf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NumDesc {
    nonneg: bool,
    bits: Bits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuiltinT {
    Tag,
    Row,
    Record,
    List,
    Bool,
    TypePlus,
    Eq,
    Refl,
    RecordGet,
    RecordKeepFields,
    RecordDropFields,
    ListLength,
    ListIndexL,
    NatFold,
    If,
    IntGte0,
    IntEq,
    IntNeq,
    TagEq,
    W,
    Wrap,
    Unwrap,
    Never,
    Any,
    Add(NumDesc),
    Sub(NumDesc),
    Num(NumDesc),
    IntNeg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tag(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    NumLit(i64),
    TagLit(Tag),
    BoolLit(bool),
    ListLit(Option<Arc<[Value]>>),
    RecordLit(Option<Arc<[(u64, Arc<Value>)]>>),
    Lambda(InstrPtr),
    Closure(Arc<Closure>),
    BuiltinsVar,
    Builtin(BuiltinT),
    Panic,
    // Pi(Quant, TermT, Either<(Ident, Lambda<TermT>), TermT>),
    // Concat(TermT, Either<(Ident, Lambda<TermT>), TermT>),
}

type InstrPtr = NonNull<u8>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Closure(Vec<Value>, InstrPtr);

/* Instructions
0) Push(Value)
1) Copy(u64)
2) App
...
*/

// #[derive(Debug, Clone, PartialEq, Eq)]
// enum Instr {
//     Push(Arc<Value>),
//     Copy(u64),
//     App,
//     PackList(u64),
//     PackRecord(u64),
//     MkLambda(u64, Box<[Instr]>), // 24 bytes, awful
// }

// // fn eval() -> Value

fn main() {
    assert_eq!(size_of::<Value>(), 16); // Awful
    // assert_eq!(size_of::<Instr>(), 32); // Awful, oh god
}
