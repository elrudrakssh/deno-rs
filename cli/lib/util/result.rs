// Copyright 2018-2025 the Deno authors. MIT license.

use std::convert::Infallible;
use std::fmt::Debug;
use std::fmt::Display;

use deno_error::JsErrorBox;
use deno_error::JsErrorClass;
use deno_runtime::deno_core::error::AnyError;
use deno_runtime::deno_core::error::CoreError;
use deno_runtime::deno_core::error::CoreErrorKind;

pub trait InfallibleResultExt<T> {
  fn unwrap_infallible(self) -> T;
}

impl<T> InfallibleResultExt<T> for Result<T, Infallible> {
  fn unwrap_infallible(self) -> T {
    match self {
      Ok(value) => value,
      Err(never) => match never {},
    }
  }
}

pub fn js_error_downcast_ref(
  err: &AnyError,
) -> Option<&deno_runtime::deno_core::error::JsError> {
  any_and_jserrorbox_downcast_ref(err).or_else(|| {
    err
      .downcast_ref::<CoreError>()
      .and_then(|e| match e.as_kind() {
        CoreErrorKind::Js(e) => Some(e),
        _ => None,
      })
  })
}

pub fn any_and_jserrorbox_downcast_ref<
  E: Display + Debug + Send + Sync + 'static,
>(
  err: &AnyError,
) -> Option<&E> {
  err
    .downcast_ref::<E>()
    .or_else(|| {
      err
        .downcast_ref::<JsErrorBox>()
        .and_then(|e| e.as_any().downcast_ref::<E>())
    })
    .or_else(|| {
      err
        .downcast_ref::<CoreError>()
        .and_then(|e| match e.as_kind() {
          CoreErrorKind::JsBox(e) => e.as_any().downcast_ref::<E>(),
          _ => None,
        })
    })
}
