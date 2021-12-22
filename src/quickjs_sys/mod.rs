#[macro_use]
mod macros;
mod js_module;

use std::collections::HashMap;

pub use js_module::{
    JsClassDef, JsClassGetterSetter, JsClassProto, JsMethod, JsModuleDef, ModuleInit,
};

#[allow(warnings)]
mod qjs {
    include!("../../lib/binding.rs");
}

use qjs::*;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::ops::DerefMut;

struct DroppableValue<T, F>
where
    F: FnMut(&mut T),
{
    value: T,
    drop_fn: F,
}

impl<T, F> DroppableValue<T, F>
where
    F: FnMut(&mut T),
{
    pub fn new(value: T, drop_fn: F) -> Self {
        Self { value, drop_fn }
    }
}

impl<T, F> Drop for DroppableValue<T, F>
where
    F: FnMut(&mut T),
{
    fn drop(&mut self) {
        (self.drop_fn)(&mut self.value);
    }
}

impl<T, F> std::ops::Deref for DroppableValue<T, F>
where
    F: FnMut(&mut T),
{
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T, F> std::ops::DerefMut for DroppableValue<T, F>
where
    F: FnMut(&mut T),
{
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

pub trait JsFn {
    fn call(ctx: &mut Context, this_val: JsValue, argv: &[JsValue]) -> JsValue;
}

struct JsFunctionTrampoline;
impl JsFunctionTrampoline {
    // How i figured it out!
    unsafe extern "C" fn callback<T: JsFn>(
        ctx: *mut JSContext,
        this_obj: JSValue,
        len: ::std::os::raw::c_int,
        argv: *mut JSValue,
    ) -> JSValue {
        let mut n_ctx = std::mem::ManuallyDrop::new(Context {
            rt: JS_GetRuntime(ctx),
            ctx,
        });
        let n_ctx = n_ctx.deref_mut();
        let this_obj = JsValue::from_qjs_value(ctx, JS_DupValue_real(ctx, this_obj));
        let mut arg_vec = vec![];
        for i in 0..len {
            let arg = argv.offset(i as isize);
            let v = *arg;
            let v = JsValue::from_qjs_value(ctx, JS_DupValue_real(ctx, v));
            arg_vec.push(v);
        }
        let r = T::call(n_ctx, this_obj, arg_vec.as_slice());
        r.into_qjs_value()
    }
}

pub struct Context {
    rt: *mut JSRuntime,
    ctx: *mut JSContext,
}

fn get_file_name(ctx: &mut Context, n_stack_levels: usize) -> JsValue {
    unsafe {
        let basename = JS_GetScriptOrModuleName(ctx.ctx, n_stack_levels as i32);
        if basename == JS_ATOM_NULL {
            JsValue::Null
        } else {
            let basename_val = JS_AtomToValue(ctx.ctx, basename);
            JsValue::from_qjs_value(ctx.ctx, basename_val)
        }
    }
}

fn js_init_cjs(ctx: &mut Context) {
    struct JsRequire;
    impl JsFn for JsRequire {
        fn call(ctx: &mut Context, _this_val: JsValue, argv: &[JsValue]) -> JsValue {
            unsafe {
                if let Some(JsValue::String(specifier)) = argv.get(0) {
                    let mut specifier = specifier.to_string();
                    if specifier.starts_with('.') {
                        if let JsValue::String(file_name) = get_file_name(ctx, 1) {
                            let file_name = file_name.to_string();
                            let mut p = std::path::PathBuf::from(file_name);
                            p.pop();
                            p.push(specifier);
                            specifier = format!("{}", p.display())
                        }
                    }

                    let m = JsValue::from_qjs_value(
                        ctx.ctx,
                        js_require(ctx.ctx, ctx.new_string(specifier.as_str()).0.v),
                    );
                    let global = ctx.get_global();
                    if let JsValue::Object(mut module) = global.get("module") {
                        let exports = module.get("exports");
                        match exports {
                            JsValue::Null | JsValue::UnDefined => m,
                            exports => {
                                module.delete("exports");
                                exports
                            }
                        }
                    } else {
                        m
                    }
                } else {
                    JsValue::UnDefined
                }
            }
        }
    }
    struct JsDirName;
    impl JsFn for JsDirName {
        fn call(ctx: &mut Context, _this_val: JsValue, _argv: &[JsValue]) -> JsValue {
            if let JsValue::String(file_name) = get_file_name(ctx, 1) {
                let file_name = file_name.to_string();
                let p = std::path::Path::new(file_name.as_str());
                if let Some(parent) = p.parent() {
                    ctx.new_string(format!("{}", parent.display()).as_str())
                        .into()
                } else {
                    JsValue::Null
                }
            } else {
                JsValue::Null
            }
        }
    }

    let mut global = ctx.get_global();
    global.set("module", ctx.new_object().into());
    global.set("require", ctx.new_function::<JsRequire>("require").into());
    let get_dirname: JsValue = ctx.new_function::<JsDirName>("get_dirname").into();
    unsafe {
        let ctx = ctx.ctx;
        JS_DefineProperty(
            ctx,
            global.0.v,
            JS_NewAtom(ctx, "__dirname\0".as_ptr().cast()),
            js_undefined(),
            get_dirname.get_qjs_value(),
            js_null(),
            (JS_PROP_THROW
                | JS_PROP_HAS_ENUMERABLE
                | JS_PROP_ENUMERABLE
                | JS_PROP_HAS_CONFIGURABLE
                | JS_PROP_CONFIGURABLE
                | JS_PROP_HAS_GET) as i32,
        )
    };
}

impl Context {
    pub fn new() -> Context {
        unsafe {
            let rt = JS_NewRuntime();
            JS_SetModuleLoaderFunc(rt, None, Some(js_module_loader), 0 as *mut std::ffi::c_void);
            js_std_init_handlers(rt);
            let ctx = JS_NewContext(rt);
            JS_AddIntrinsicBigFloat(ctx);
            JS_AddIntrinsicBigDecimal(ctx);
            JS_AddIntrinsicOperators(ctx);
            JS_EnableBignumExt(ctx, 1);
            js_std_add_console(ctx);
            js_init_module_std(ctx, "std\0".as_ptr() as *const i8);
            js_init_module_os(ctx, "os\0".as_ptr() as *const i8);
            let mut ctx = Context { rt, ctx };
      
  
            #[cfg(feature = "cjs")]
            {
                js_init_cjs(&mut ctx);
            }
            ctx
        }
    }

    pub fn get_global(&mut self) -> JsObject {
        unsafe {
            let v = JS_GetGlobalObject(self.ctx);
            JsObject(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn put_args<T, I>(&mut self, args: T)
    where
        T: AsRef<[I]>,
        I: AsRef<str>,
    {
        let mut args_obj = self.new_array();
        let args = args.as_ref();
        let mut i = 0;
        for arg in args {
            let arg = arg.as_ref();
            let arg_js_string = self.new_string(arg);
            args_obj.set(i, arg_js_string.into());
            i += 1;
        }
        let mut global = self.get_global();
        global.set("args", args_obj.into());
    }

    pub fn eval_buf(&mut self, code: &str, filename: &str, eval_flags: u32) -> JsValue {
        unsafe {
            let ctx = self.ctx;
            let val = if (eval_flags & JS_EVAL_TYPE_MASK) == JS_EVAL_TYPE_MODULE {
                let val = JS_Eval(
                    ctx,
                    make_c_string(code).as_ptr(),
                    code.len(),
                    make_c_string(filename).as_ptr(),
                    (eval_flags | JS_EVAL_FLAG_COMPILE_ONLY) as i32,
                );
                if JS_IsException_real(val) <= 0 {
                    JS_EvalFunction(ctx, val)
                } else {
                    val
                }
            } else {
                JS_Eval(
                    ctx,
                    make_c_string(code).as_ptr(),
                    code.len(),
                    make_c_string(filename).as_ptr(),
                    eval_flags as i32,
                )
            };
            if JS_IsException_real(val) > 0 {
                js_std_dump_error(ctx);
            }
            JsValue::from_qjs_value(ctx, val)
        }
    }

    pub fn eval_global_str(&mut self, code: &str) -> JsValue {
        self.eval_buf(code, "<evalScript>", JS_EVAL_TYPE_GLOBAL)
    }

    pub fn eval_module_str(&mut self, code: &str, filename: &str) {
        self.eval_buf(code, filename, JS_EVAL_TYPE_MODULE);
        self.promise_loop_poll();
    }

    pub fn new_function<F: JsFn>(&mut self, name: &str) -> JsFunction {
        unsafe {
            let name = std::ffi::CString::new(name).unwrap();
            let v = JS_NewCFunction_real(
                self.ctx,
                Some(JsFunctionTrampoline::callback::<F>),
                name.as_ptr(),
                1,
            );
            JsFunction(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn new_object(&mut self) -> JsObject {
        let v = unsafe { JS_NewObject(self.ctx) };
        JsObject(JsRef { ctx: self.ctx, v })
    }

    pub fn new_array(&mut self) -> JsArray {
        unsafe {
            let v = JS_NewArray(self.ctx);
            JsArray(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn new_array_buffer(&mut self, buff: &[u8]) -> JsArrayBuffer {
        unsafe {
            let v = JS_NewArrayBufferCopy(self.ctx, buff.as_ptr() as *const u8, buff.len());
            JsArrayBuffer(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn new_array_buffer_t<T: Sized>(&mut self, buff: &[T]) -> JsArrayBuffer {
        unsafe {
            let v = JS_NewArrayBufferCopy(
                self.ctx,
                buff.as_ptr() as *const u8,
                buff.len() * std::mem::size_of::<T>(),
            );
            JsArrayBuffer(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn new_string(&mut self, s: &str) -> JsString {
        unsafe {
            let v = JS_NewStringLen(self.ctx, s.as_ptr() as *const i8, s.len());
            JsString(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn value_to_string(&mut self, v: &JsValue) -> JsValue {
        unsafe {
            let v = JS_ToString(self.ctx, v.get_qjs_value());
            JsValue::from_qjs_value(self.ctx, v)
        }
    }

    pub fn new_error(&mut self, msg: &str) -> JsValue {
        let msg = self.new_string(msg);
        let error = unsafe { JS_NewError(self.ctx) };
        let mut error_obj = JsValue::from_qjs_value(self.ctx, error);
        if let JsValue::Object(o) = &mut error_obj {
            o.set("message", msg.into());
        };
        error_obj
    }

    pub fn throw_type_error(&mut self, msg: &str) -> JsException {
        unsafe {
            let v = JS_ThrowTypeError(self.ctx, make_c_string(msg).as_ptr());
            JsException(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn throw_error(&mut self, obj: JsValue) -> JsException {
        unsafe {
            let v = JS_Throw(self.ctx, obj.into_qjs_value());
            JsException(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn throw_internal_type_error(&mut self, msg: &str) -> JsException {
        unsafe {
            let v = JS_ThrowInternalError(self.ctx, make_c_string(msg).as_ptr());
            JsException(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn throw_reference_error(&mut self, msg: &str) -> JsException {
        unsafe {
            let v = JS_ThrowReferenceError(self.ctx, make_c_string(msg).as_ptr());
            JsException(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn throw_range_error(&mut self, msg: &str) -> JsException {
        unsafe {
            let v = JS_ThrowRangeError(self.ctx, make_c_string(msg).as_ptr());
            JsException(JsRef { ctx: self.ctx, v })
        }
    }

    pub fn new_promise(&mut self) -> (JsValue, JsValue, JsValue) {
        unsafe {
            let ctx = self.ctx;
            let mut resolving_funcs = [0, 0];

            let p = JS_NewPromiseCapability(ctx, resolving_funcs.as_mut_ptr());
            (
                JsValue::from_qjs_value(ctx, p),
                JsValue::from_qjs_value(ctx, resolving_funcs[0]),
                JsValue::from_qjs_value(ctx, resolving_funcs[1]),
            )
        }
    }

    pub fn promise_loop_poll(&mut self) {
        unsafe { js_std_loop(self.ctx) }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            js_std_free_handlers(self.rt);
            JS_FreeContext(self.ctx);
            JS_FreeRuntime(self.rt);
        }
    }
}

unsafe fn to_u32(ctx: *mut JSContext, v: JSValue) -> Result<u32, String> {
    if JS_VALUE_GET_NORM_TAG_real(v) == JS_TAG_INT {
        let mut r = 0u32;
        JS_ToUint32_real(ctx, &mut r as *mut u32, v);
        Ok(r)
    } else {
        Err("value is Not Int".into())
    }
}

pub(crate) fn make_c_string<T: Into<Vec<u8>>>(s: T) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or(Default::default())
}

pub struct JsRef {
    ctx: *mut JSContext,
    v: JSValue,
}

impl Debug for JsRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        unsafe {
            let ctx = self.ctx;
            let v = self.v;

            let ptr = JS_ToCStringLen2(ctx, std::ptr::null_mut(), v, 0);
            let s = if ptr.is_null() {
                String::new()
            } else {
                let cstr = std::ffi::CStr::from_ptr(ptr);
                let s = cstr.to_str().map(|s| s.to_string()).unwrap_or_default();
                JS_FreeCString(ctx, ptr);
                s
            };

            write!(f, "{}", s)
        }
    }
}

impl Clone for JsRef {
    fn clone(&self) -> Self {
        unsafe {
            Self {
                ctx: self.ctx,
                v: JS_DupValue_real(self.ctx, self.v),
            }
        }
    }
}

impl Drop for JsRef {
    fn drop(&mut self) {
        unsafe {
            let tag = JS_VALUE_GET_NORM_TAG_real(self.v);
            match tag {
                JS_TAG_STRING
                | JS_TAG_OBJECT
                | JS_TAG_FUNCTION_BYTECODE
                | JS_TAG_BIG_INT
                | JS_TAG_BIG_FLOAT
                | JS_TAG_BIG_DECIMAL
                | JS_TAG_SYMBOL => JS_FreeValue_real(self.ctx, self.v),
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsObject(JsRef);

impl JsObject {
    pub fn get(&self, key: &str) -> JsValue {
        unsafe {
            let js_ref = &self.0;
            let ctx = js_ref.ctx;
            let v = js_ref.v;
            let r = JS_GetPropertyStr(ctx, v, make_c_string(key).as_ptr().cast());
            JsValue::from_qjs_value(ctx, r)
        }
    }

    pub fn set(&mut self, key: &str, value: JsValue) -> JsValue {
        unsafe {
            let js_ref = &self.0;
            let ctx = js_ref.ctx;
            let this_obj = js_ref.v;
            let v = value.into_qjs_value();
            if JS_SetPropertyStr(ctx, this_obj, make_c_string(key).as_ptr().cast(), v) != 0 {
                JsValue::Exception(JsException(JsRef {
                    ctx,
                    v: js_exception(),
                }))
            } else {
                JsValue::UnDefined
            }
        }
    }

    pub fn invoke(&mut self, fn_name: &str, argv: &mut [JsValue]) -> JsValue {
        unsafe {
            let ctx = self.0.ctx;
            let this_obj = self.0.v;
            let mut argv: Vec<JSValue> = argv.iter().map(|v| v.get_qjs_value()).collect();
            let fn_name = JS_NewAtom(ctx, make_c_string(fn_name).as_ptr());
            let v = JS_Invoke(ctx, this_obj, fn_name, argv.len() as i32, argv.as_mut_ptr());
            JS_FreeAtom(ctx, fn_name);
            JsValue::from_qjs_value(ctx, v)
        }
    }

    pub fn delete(&mut self, key: &str) {
        unsafe {
            let ctx = self.0.ctx;
            let this_obj = self.0.v;
            let prop_name = JS_NewAtom(ctx, make_c_string(key).as_ptr());
            JS_DeleteProperty(ctx, this_obj, prop_name, 0);
            JS_FreeAtom(ctx, prop_name);
        }
    }

    pub fn to_map(&self) -> Result<HashMap<String, JsValue>, JsException> {
        unsafe {
            let ctx = self.0.ctx;
            let obj = self.0.v;

            let mut properties: *mut JSPropertyEnum = std::ptr::null_mut();
            let mut count: u32 = 0;

            let flags = (JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK | JS_GPN_ENUM_ONLY) as i32;
            let ret = JS_GetOwnPropertyNames(ctx, &mut properties, &mut count, obj, flags);
            if ret != 0 {
                return Err(JsException(JsRef {
                    ctx,
                    v: js_exception(),
                }));
            }

            let properties = DroppableValue::new(properties, |&mut properties| {
                for index in 0..count {
                    let prop = properties.offset(index as isize);
                    JS_FreeAtom(ctx, (*prop).atom);
                }
                js_free(ctx, properties as *mut std::ffi::c_void);
            });

            let mut map = HashMap::new();
            for index in 0..count {
                let prop = (*properties).offset(index as isize);
                let raw_value = JS_GetPropertyInternal(ctx, obj, (*prop).atom, obj, 0);
                let value = JsValue::from_qjs_value(ctx, raw_value);
                if let JsValue::Exception(e) = value {
                    return Err(e);
                }

                let key_value = JsValue::from_qjs_value(ctx, JS_AtomToString(ctx, (*prop).atom));
                if let JsValue::Exception(e) = key_value {
                    return Err(e);
                }
                if let JsValue::String(key_res) = key_value {
                    let key = key_res.to_string();
                    map.insert(key, value);
                }
            }
            Ok(map)
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsFunction(JsRef);

impl JsFunction {
    pub fn call(&self, argv: &mut [JsValue]) -> JsValue {
        unsafe {
            let ctx = self.0.ctx;
            let mut argv: Vec<JSValue> = argv.iter().map(|v| v.get_qjs_value()).collect();
            let f = self.0.v;
            let v = JS_Call(ctx, f, js_undefined(), argv.len() as i32, argv.as_mut_ptr());
            JsValue::from_qjs_value(ctx, v)
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsPromise(JsRef);

impl JsPromise {
    pub fn get_result(&self) -> JsValue {
        unsafe {
            let ctx = self.0.ctx;
            let this_obj = self.0.v;
            let v = JS_GetPromiseResult_real(ctx, this_obj);
            JsValue::from_qjs_value(ctx, v)
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsArray(JsRef);

impl JsArray {
    pub fn to_vec(&self) -> Result<Vec<JsValue>, JsException> {
        unsafe {
            let js_ref = &self.0;
            let ctx = js_ref.ctx;
            let v = js_ref.v;
            let len_raw = JS_GetPropertyStr(ctx, v, make_c_string("length").as_ptr());

            let len = to_u32(ctx, len_raw).unwrap_or(0);
            JS_FreeValue_real(ctx, len_raw);

            let mut values = Vec::new();
            for index in 0..(len as usize) {
                let value_raw = JS_GetPropertyUint32(ctx, v, index as u32);
                if JS_VALUE_GET_NORM_TAG_real(value_raw) == JS_TAG_EXCEPTION {
                    return Err(JsException(JsRef { ctx, v: value_raw }));
                }
                let v = JsValue::from_qjs_value(ctx, value_raw);
                values.push(v);
            }
            Ok(values)
        }
    }
    pub fn set_length(&mut self, len: usize) -> bool {
        unsafe {
            let ctx = self.0.ctx;
            let v = self.0.v;
            let b = JS_SetPropertyStr(
                ctx,
                v,
                make_c_string("length").as_ptr().cast(),
                JS_NewInt64_real(ctx, len as i64),
            );
            b == 0
        }
    }
    pub fn get_length(&self) -> usize {
        unsafe {
            let ctx = self.0.ctx;
            let v = self.0.v;
            let len = JS_GetPropertyStr(ctx, v, make_c_string("length").as_ptr().cast());
            to_u32(ctx, len).unwrap_or(0) as usize
        }
    }
    pub fn get(&self, i: usize) -> JsValue {
        unsafe {
            let ctx = self.0.ctx;
            let this_obj = self.0.v;
            let v = JS_GetPropertyUint32(ctx, this_obj, i as u32);
            JsValue::from_qjs_value(ctx, v)
        }
    }
    pub fn set(&mut self, i: usize, v: JsValue) {
        unsafe {
            let ctx = self.0.ctx;
            let this_obj = self.0.v;
            let v = v.into_qjs_value();
            JS_SetPropertyUint32(ctx, this_obj, i as u32, v);
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsArrayBuffer(JsRef);

impl JsArrayBuffer {
    pub fn to_vec(&self) -> Vec<u8> {
        unsafe {
            let (p, len) = self.get_mut_ptr();
            if len == 0 {
                Vec::new()
            } else {
                let mut r = vec![0u8; len];
                p.copy_to(r.as_mut_ptr(), len);
                r
            }
        }
    }
    pub fn get_mut_ptr(&self) -> (*mut u8, usize) {
        unsafe {
            let r = &self.0;
            let mut len = 0;
            let p = JS_GetArrayBuffer(r.ctx, &mut len, r.v);
            (p, len)
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsString(JsRef);

impl JsString {
    pub fn to_string(&self) -> String {
        unsafe {
            let r = &self.0;
            let ptr = JS_ToCStringLen2(r.ctx, std::ptr::null_mut(), r.v, 0);
            if ptr.is_null() {
                return String::new();
            }
            let cstr = std::ffi::CStr::from_ptr(ptr);
            let s = cstr.to_str().map(|s| s.to_string()).unwrap_or_default();
            JS_FreeCString(r.ctx, ptr);
            s
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsModule(JsRef);

#[derive(Debug, Clone)]
pub struct JsFunctionByteCode(JsRef);

#[derive(Debug, Clone)]
pub struct JsBigNum(JsRef);

impl JsBigNum {
    pub fn to_int64(&self) -> i64 {
        unsafe {
            let mut v = 0_i64;
            JS_ToBigInt64(self.0.ctx, (&mut v) as *mut i64, self.0.v);
            v
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsException(JsRef);

impl JsException {
    pub fn dump_error(&self) {
        unsafe { js_std_dump_error(self.0.ctx) }
    }
}

#[derive(Debug, Clone)]
pub enum JsValue {
    Int(i32),
    Float(f64),
    BigNum(JsBigNum),
    String(JsString),
    Module(JsModule),
    Object(JsObject),
    Array(JsArray),
    Promise(JsPromise),
    ArrayBuffer(JsArrayBuffer),
    Function(JsFunction),
    Bool(bool),
    Null,
    UnDefined,
    Exception(JsException),
    FunctionByteCode(JsFunctionByteCode),
    Other(JsRef),
}

impl JsValue {
    fn from_qjs_value(ctx: *mut JSContext, v: JSValue) -> Self {
        unsafe {
            let tag = JS_VALUE_GET_NORM_TAG_real(v);
            match tag {
                JS_TAG_INT => {
                    let mut num = 0;
                    JS_ToInt32(ctx, (&mut num) as *mut i32, v);
                    JsValue::Int(num)
                }
                JS_TAG_FLOAT64 => {
                    let mut num = 0_f64;
                    JS_ToFloat64(ctx, (&mut num) as *mut f64, v);
                    JsValue::Float(num)
                }
                JS_TAG_BIG_DECIMAL | JS_TAG_BIG_INT | JS_TAG_BIG_FLOAT => {
                    JsValue::BigNum(JsBigNum(JsRef { ctx, v }))
                }
                JS_TAG_STRING => JsValue::String(JsString(JsRef { ctx, v })),
                JS_TAG_MODULE => JsValue::Module(JsModule(JsRef { ctx, v })),
                JS_TAG_OBJECT => {
                    if JS_IsFunction(ctx, v) != 0 {
                        JsValue::Function(JsFunction(JsRef { ctx, v }))
                    } else if JS_IsArrayBuffer(ctx, v) != 0 {
                        JsValue::ArrayBuffer(JsArrayBuffer(JsRef { ctx, v }))
                    } else if JS_IsArray(ctx, v) != 0 {
                        JsValue::Array(JsArray(JsRef { ctx, v }))
                    } else if JS_IsPromise(ctx, v) != 0 {
                        JsValue::Promise(JsPromise(JsRef { ctx, v }))
                    } else {
                        JsValue::Object(JsObject(JsRef { ctx, v }))
                    }
                }
                JS_TAG_BOOL => JsValue::Bool(JS_ToBool(ctx, v) != 0),
                JS_TAG_NULL => JsValue::Null,
                JS_TAG_EXCEPTION => JsValue::Exception(JsException(JsRef { ctx, v })),
                JS_TAG_UNDEFINED => JsValue::UnDefined,
                JS_TAG_FUNCTION_BYTECODE => {
                    JsValue::FunctionByteCode(JsFunctionByteCode(JsRef { ctx, v }))
                }
                _ => JsValue::Other(JsRef { ctx, v }),
            }
        }
    }
    fn get_qjs_value(&self) -> JSValue {
        unsafe {
            match self {
                // JS_NewInt32 dont need ctx
                JsValue::Int(v) => JS_NewInt32_real(std::ptr::null_mut(), *v),
                // JS_NewFloat64 dont need ctx
                JsValue::Float(v) => JS_NewFloat64_real(std::ptr::null_mut(), *v),
                JsValue::BigNum(JsBigNum(JsRef { v, .. })) => *v,
                JsValue::String(JsString(JsRef { v, .. })) => *v,
                JsValue::Module(JsModule(JsRef { v, .. })) => *v,
                JsValue::Object(JsObject(JsRef { v, .. })) => *v,
                JsValue::Array(JsArray(JsRef { v, .. })) => *v,
                JsValue::ArrayBuffer(JsArrayBuffer(JsRef { v, .. })) => *v,
                JsValue::Function(JsFunction(JsRef { v, .. })) => *v,
                JsValue::Promise(JsPromise(JsRef { v, .. })) => *v,
                JsValue::Bool(b) => JS_NewBool_real(std::ptr::null_mut(), if *b { 1 } else { 0 }),
                JsValue::Null => js_null(),
                JsValue::UnDefined => js_undefined(),
                JsValue::Exception(JsException(JsRef { v, .. })) => *v,
                JsValue::FunctionByteCode(JsFunctionByteCode(JsRef { v, .. })) => *v,
                JsValue::Other(JsRef { v, .. }) => *v,
            }
        }
    }

    fn into_qjs_value(self) -> JSValue {
        let s = std::mem::ManuallyDrop::new(self);
        s.get_qjs_value()
    }
}

impl From<i32> for JsValue {
    fn from(v: i32) -> Self {
        Self::Int(v)
    }
}

impl From<f64> for JsValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<JsBigNum> for JsValue {
    fn from(v: JsBigNum) -> Self {
        Self::BigNum(v)
    }
}

impl From<JsString> for JsValue {
    fn from(v: JsString) -> Self {
        Self::String(v)
    }
}

impl From<JsModule> for JsValue {
    fn from(v: JsModule) -> Self {
        Self::Module(v)
    }
}

impl From<JsObject> for JsValue {
    fn from(v: JsObject) -> Self {
        Self::Object(v)
    }
}

impl From<JsArray> for JsValue {
    fn from(v: JsArray) -> Self {
        Self::Array(v)
    }
}

impl From<JsPromise> for JsValue {
    fn from(v: JsPromise) -> Self {
        Self::Promise(v)
    }
}

impl From<JsArrayBuffer> for JsValue {
    fn from(v: JsArrayBuffer) -> Self {
        Self::ArrayBuffer(v)
    }
}

impl From<JsFunction> for JsValue {
    fn from(v: JsFunction) -> Self {
        Self::Function(v)
    }
}

impl From<bool> for JsValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<JsException> for JsValue {
    fn from(v: JsException) -> Self {
        Self::Exception(v)
    }
}

impl From<JsFunctionByteCode> for JsValue {
    fn from(v: JsFunctionByteCode) -> Self {
        Self::FunctionByteCode(v)
    }
}

impl From<JsRef> for JsValue {
    fn from(v: JsRef) -> Self {
        Self::from_qjs_value(v.ctx, v.v)
    }
}
