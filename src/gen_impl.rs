//! # generator
//!
//! Rust generator implementation
//!

use std::any::Any;
use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::panic;
use std::thread;

use crate::reg_context::RegContext;
use crate::rt::{Context, ContextStack, Error};
use crate::scope::Scope;
use crate::stack::{Func, Stack, StackBox};
use crate::yield_::yield_now;

// default stack size, in usize
// windows has a minimal size as 0x4a8!!!!
pub const DEFAULT_STACK_SIZE: usize = 0x1000;

/// the generator type
pub struct Generator<'a, A, T> {
    _stack: StackBox<Stack>,
    gen: ManuallyDrop<StackBox<GeneratorImpl<'a, A, T>>>,
}

unsafe impl<A, T> Send for Generator<'static, A, T> {}

impl<'a, A, T> Generator<'a, A, T> {
    /// Constructs a Generator from a raw pointer.
    ///
    /// # Safety
    ///
    /// This function is unsafe because improper use may lead to
    /// memory problems. For example, a double-free may occur if the
    /// function is called twice on the same raw pointer.
    #[inline]
    pub unsafe fn from_raw(raw: *mut usize) -> Self {
        let g = StackBox::from_raw(raw as *mut GeneratorImpl<'a, A, T>);
        let stack_ptr = raw.offset(g.size() as isize + 2);
        Generator {
            _stack: StackBox::from_raw(stack_ptr as *mut Stack),
            gen: ManuallyDrop::new(g),
        }
    }

    /// Consumes the `Generator`, returning a wrapped raw pointer.
    #[inline]
    pub fn into_raw(g: Generator<'a, A, T>) -> *mut usize {
        let ret = g.gen.as_ptr() as *mut usize;
        std::mem::forget(g);
        ret
    }
}

impl<'a, A, T> std::ops::Deref for Generator<'a, A, T> {
    type Target = GeneratorImpl<'a, A, T>;

    fn deref(&self) -> &GeneratorImpl<'a, A, T> {
        &*self.gen
    }
}

impl<'a, A, T> std::ops::DerefMut for Generator<'a, A, T> {
    fn deref_mut(&mut self) -> &mut GeneratorImpl<'a, A, T> {
        &mut *self.gen
    }
}

impl<'a, T> Iterator for Generator<'a, (), T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.resume()
    }
}

impl<'a, A, T> fmt::Debug for Generator<'a, A, T> {
    #[cfg(nightly)]
    #[allow(unused_unsafe)]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use std::intrinsics::type_name;
        write!(
            f,
            "Generator<{}, Output={}> {{ ... }}",
            unsafe { type_name::<A>() },
            unsafe { type_name::<T>() }
        )
    }

    #[cfg(not(nightly))]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Generator {{ ... }}")
    }
}

impl<'a, A, T> Drop for Generator<'a, A, T> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.gen) }
    }
}

/// Generator helper
pub struct Gn<A = ()> {
    dummy: PhantomData<A>,
}

impl<A> Gn<A> {
    /// create a scoped generator with default stack size
    pub fn new_scoped<'a, T, F>(f: F) -> Generator<'a, A, T>
    where
        F: FnOnce(Scope<A, T>) -> T + 'a,
        T: 'a,
        A: 'a,
    {
        Self::new_scoped_opt(DEFAULT_STACK_SIZE, f)
    }

    /// create a scoped generator with specified stack size
    pub fn new_scoped_opt<'a, T, F>(size: usize, f: F) -> Generator<'a, A, T>
    where
        F: FnOnce(Scope<A, T>) -> T + 'a,
        T: 'a,
        A: 'a,
    {
        let mut stack = Stack::new(size);
        let mut g = GeneratorImpl::<A, T>::new(&mut stack);
        g.scoped_init(f);
        Generator {
            _stack: stack,
            gen: ManuallyDrop::new(g),
        }
    }
}

impl<A: Any> Gn<A> {
    /// create a new generator with default stack size
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::new_ret_no_self))]
    #[deprecated(since = "0.6.18", note = "please use `scope` version instead")]
    pub fn new<'a, T: Any, F>(f: F) -> Generator<'a, A, T>
    where
        F: FnOnce() -> T + 'a,
    {
        Self::new_opt(DEFAULT_STACK_SIZE, f)
    }

    /// create a new generator with specified stack size
    // the `may` library use this API so we can't deprecated it yet.
    pub fn new_opt<'a, T: Any, F>(size: usize, f: F) -> Generator<'a, A, T>
    where
        F: FnOnce() -> T + 'a,
    {
        let mut stack = Stack::new(size);
        let mut g = GeneratorImpl::<A, T>::new(&mut stack);
        g.init_context();
        g.init_code(f);
        Generator {
            _stack: stack,
            gen: ManuallyDrop::new(g),
        }
    }
}

/// `GeneratorImpl`
#[repr(C)]
pub struct GeneratorImpl<'a, A, T> {
    // run time context
    context: Context,
    // save the input
    para: Option<A>,
    // save the output
    ret: Option<T>,
    // boxed functor
    f: Option<Func>,
    // phantom lifetime
    phantom: PhantomData<&'a T>,
}

impl<'a, A: Any, T: Any> GeneratorImpl<'a, A, T> {
    /// create a new generator with default stack size
    fn init_context(&mut self) {
        unsafe {
            std::ptr::write(
                self.context.para.as_mut_ptr(),
                &mut self.para as &mut dyn Any,
            );
            std::ptr::write(self.context.ret.as_mut_ptr(), &mut self.ret as &mut dyn Any);
        }
    }
}

impl<'a, A, T> GeneratorImpl<'a, A, T> {
    /// create a new generator with specified stack size
    fn new(stack: &mut Stack) -> StackBox<Self> {
        unsafe {
            let mut stack_box = stack
                .alloc_uninit_box::<GeneratorImpl<'a, A, T>>()
                .assume_init();

            stack_box.init(GeneratorImpl {
                para: None,
                ret: None,
                f: None,
                context: Context::new(stack),
                phantom: PhantomData,
            });
            stack_box
        }
    }

    /// prefetch the generator into cache
    #[inline]
    pub fn prefetch(&self) {
        self.context.regs.prefetch();
    }

    /// init a heap based generator with scoped closure
    pub fn scoped_init<F: FnOnce(Scope<'a, A, T>) -> T + 'a>(&mut self, f: F)
    where
        T: 'a,
        A: 'a,
    {
        use std::mem::transmute;
        let scope = unsafe { transmute(Scope::new(&mut self.para, &mut self.ret)) };
        self.init_code(move || f(scope));
    }

    /// init a heap based generator
    // it's can be used to re-init a 'done' generator before it's get dropped
    pub fn init_code<F: FnOnce() -> T + 'a>(&mut self, f: F)
    where
        T: 'a,
    {
        // make sure the last one is finished
        if self.f.is_none() && self.context._ref == 0 {
            unsafe { self.cancel() };
        } else {
            let _ = self.f.take();
        }

        // init ctx parent to itself, this would be the new top
        self.context.parent = &mut self.context;

        // init the ref to 0 means that it's ready to start
        self.context._ref = 0;
        let stack = unsafe { &mut *self.context.stack };
        let ret = &mut self.ret as *mut _;
        let context = &mut self.context as *mut Context;
        // alloc the function on stack
        let func = StackBox::new_fn_once(stack, move || {
            let r = f();
            let ret = unsafe { &mut *ret };
            let _ref = unsafe { (*context)._ref };
            if _ref == 0xf {
                ::std::mem::forget(r);
                *ret = None; // this is a done return
            } else {
                *ret = Some(r); // normal return
            }
        });

        self.f = Some(func);

        let stk = unsafe { &*self.context.stack };
        self.context
            .regs
            .init_with(gen_init, 0, &mut self.f as *mut _ as *mut usize, stk);
    }

    /// resume the generator
    #[inline]
    fn resume_gen(&mut self) {
        let env = ContextStack::current();
        // get the current regs
        let cur = &mut env.top().regs;

        // switch to new context, always use the top context's reg
        // for normal generator self.context.parent == self.context
        // for coroutine self.context.parent == top generator context
        debug_assert!(!self.context.parent.is_null());
        let top = unsafe { &mut *self.context.parent };

        // save current generator context on stack
        env.push_context(&mut self.context);

        // swap to the generator
        RegContext::swap(cur, &top.regs);

        // comes back, check the panic status
        // this would propagate the panic until root context
        // if it's a coroutine just stop propagate
        if !self.context.local_data.is_null() {
            return;
        }

        if let Some(err) = self.context.err.take() {
            // pass the error to the parent until root
            #[cold]
            panic::resume_unwind(err);
        }
    }

    #[inline]
    fn is_started(&self) -> bool {
        // when the f is consumed we think it's running
        self.f.is_none()
    }

    /// prepare the para that passed into generator before send
    #[inline]
    pub fn set_para(&mut self, para: A) {
        self.para = Some(para);
    }

    /// set the generator local data
    #[inline]
    pub fn set_local_data(&mut self, data: *mut u8) {
        self.context.local_data = data;
    }

    /// get the generator local data
    #[inline]
    pub fn get_local_data(&self) -> *mut u8 {
        self.context.local_data
    }

    /// get the generator panic data
    #[inline]
    pub fn get_panic_data(&mut self) -> Option<Box<dyn Any + Send>> {
        self.context.err.take()
    }

    /// resume the generator without touch the para
    /// you should call `set_para` before this method
    #[inline]
    pub fn resume(&mut self) -> Option<T> {
        if self.is_done() {
            #[cold]
            return None;
        }

        // every time we call the function, increase the ref count
        // yield will decrease it and return will not
        self.context._ref += 1;
        self.resume_gen();

        self.ret.take()
    }

    /// `raw_send`
    #[inline]
    pub fn raw_send(&mut self, para: Option<A>) -> Option<T> {
        if self.is_done() {
            #[cold]
            return None;
        }

        // this is the passed in value of the send primitive
        // the yield part would read out this value in the next round
        self.para = para;

        // every time we call the function, increase the ref count
        // yield will decrease it and return will not
        self.context._ref += 1;
        self.resume_gen();

        self.ret.take()
    }

    /// send interface
    pub fn send(&mut self, para: A) -> T {
        let ret = self.raw_send(Some(para));
        ret.expect("send got None return")
    }

    /// cancel the generator without any check
    #[inline]
    unsafe fn raw_cancel(&mut self) {
        // tell the func to panic
        // so that we can stop the inner func
        self.context._ref = 2;
        // save the old panic hook, we don't want to print anything for the Cancel
        let old = ::std::panic::take_hook();
        ::std::panic::set_hook(Box::new(|_| {}));
        self.resume_gen();
        ::std::panic::set_hook(old);
    }

    /// cancel the generator
    /// this will trigger a Cancel panic, it's unsafe in that you must care about the resource
    pub unsafe fn cancel(&mut self) {
        if self.is_done() {
            return;
        }

        // consume the fun if it's not started
        if !self.is_started() {
            self.f.take();
            self.context._ref = 1;
        } else {
            self.raw_cancel();
        }
    }

    /// is finished
    #[inline]
    pub fn is_done(&self) -> bool {
        self.is_started() && (self.context._ref & 0x3) != 0
    }

    /// get stack total size and used size in word
    pub fn stack_usage(&self) -> (usize, usize) {
        let stack = unsafe { &*self.context.stack };
        (stack.size(), stack.get_used_size())
    }
}

impl<'a, A, T> Drop for GeneratorImpl<'a, A, T> {
    fn drop(&mut self) {
        // when the thread is already panic, do nothing
        if thread::panicking() {
            return;
        }

        if !self.is_started() {
            // not started yet, just drop the gen
            return;
        }

        if !self.is_done() {
            warn!("generator is not done while drop");
            unsafe { self.raw_cancel() }
        }

        assert!(self.is_done());

        let (total_stack, used_stack) = self.stack_usage();
        if used_stack < total_stack {
            // here we should record the stack in the class
            // next time will just use
            // set_stack_size::<F>(used_stack);
        } else {
            error!("stack overflow detected!");
            panic!(Error::StackErr);
        }
    }
}

/// the init function passed to reg_context
fn gen_init(_: usize, f: *mut usize) -> ! {
    let clo = move || {
        // consume self.f
        let f: &mut Option<Func> = unsafe { &mut *(f as *mut _) };
        let func = f.take().unwrap();
        func.call_once();
    };

    fn check_err(cause: Box<dyn Any + Send + 'static>) {
        if let Some(&Error::Cancel) = cause.downcast_ref::<Error>() {
            // this is not an error at all, ignore it
            return;
        }
        error!("set panicked inside generator");
        ContextStack::current().top().err = Some(cause);
    }

    // we can't panic inside the generator context
    // need to propagate the panic to the main thread
    if let Err(cause) = panic::catch_unwind(clo) {
        check_err(cause);
    }

    yield_now();

    unreachable!("Should never comeback");
}
