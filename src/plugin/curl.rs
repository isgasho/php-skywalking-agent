// Copyright (c) 2022 jmjoy
// Helper is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2. You may obtain a copy of Mulan PSL v2 at:
//          http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use super::Plugin;
use crate::{
    component::COMPONENT_PHP_CURL_ID,
    context::RequestContext,
    execute::{AfterExecuteHook, BeforeExecuteHook},
};
use anyhow::{anyhow, bail, Context};
use phper::{
    arrays::{InsertKey, ZArray},
    functions::call,
    values::{ExecuteData, ZVal},
};
use skywalking::context::{
    propagation::encoder::encode_propagation, span::Span, trace_context::TracingContext,
};
use std::{cell::RefCell, collections::HashMap, os::raw::c_long};
use tracing::{debug, warn};
use url::Url;

static CURLOPT_HTTPHEADER: c_long = 10023;

thread_local! {
    static CURL_HEADERS: RefCell<HashMap<i64, ZVal>> = Default::default();
}

#[derive(Default, Clone)]
pub struct CurlPlugin;

impl Plugin for CurlPlugin {
    #[inline]
    fn class_names(&self) -> Option<&'static [&'static str]> {
        None
    }

    #[inline]
    fn function_name_prefix(&self) -> Option<&'static str> {
        Some("curl_")
    }

    fn hook(
        &self, _class_name: Option<&str>, function_name: &str,
    ) -> Option<(Box<BeforeExecuteHook>, Box<AfterExecuteHook>)> {
        match function_name {
            "curl_setopt" => Some(self.hook_curl_setopt()),
            "curl_setopt_array" => Some(self.execute_curl_setopt_array()),
            "curl_exec" => Some(self.execute_curl_exec()),
            "curl_close" => Some(self.execute_curl_close()),
            _ => None,
        }
    }
}

impl CurlPlugin {
    #[tracing::instrument(skip_all)]
    fn hook_curl_setopt(&self) -> (Box<BeforeExecuteHook>, Box<AfterExecuteHook>) {
        (
            Box::new(|execute_data| {
                if execute_data.num_args() < 3 {
                    bail!("argument count incorrect");
                }

                let cid = Self::get_resource_id(execute_data)?;

                if matches!(execute_data.get_parameter(1).as_long(), Some(n) if n == CURLOPT_HTTPHEADER)
                {
                    let value = execute_data.get_parameter(2);
                    if value.get_type_info().is_array() {
                        CURL_HEADERS
                            .with(|headers| headers.borrow_mut().insert(cid, value.clone()));
                    }
                }

                Ok(Box::new(()))
            }),
            Box::new(|_, _, _| Ok(())),
        )
    }

    #[tracing::instrument(skip_all)]
    fn execute_curl_setopt_array(&self) -> (Box<BeforeExecuteHook>, Box<AfterExecuteHook>) {
        (
            Box::new(|execute_data| {
                if execute_data.num_args() < 2 {
                    bail!("argument count incorrect");
                }

                let cid = Self::get_resource_id(execute_data)?;

                if let Some(opts) = execute_data.get_parameter(1).as_z_arr() {
                    if let Some(value) = opts.get(CURLOPT_HTTPHEADER as u64) {
                        CURL_HEADERS
                            .with(|headers| headers.borrow_mut().insert(cid, value.clone()));
                    }
                }

                Ok(Box::new(()))
            }),
            Box::new(|_, _, _| Ok(())),
        )
    }

    #[tracing::instrument(skip_all)]
    fn execute_curl_exec(&self) -> (Box<BeforeExecuteHook>, Box<AfterExecuteHook>) {
        (
            Box::new(|execute_data| {
                if execute_data.num_args() < 1 {
                    bail!("argument count incorrect");
                }

                let cid = Self::get_resource_id(execute_data)?;

                let ch = execute_data.get_parameter(0);
                let result =
                    call("curl_getinfo", &mut [ch.clone()]).context("Call curl_get_info failed")?;
                let result = result.as_z_arr().context("result isn't array")?;

                let url = result
                    .get("url")
                    .context("Get url from curl_get_info result failed")?;
                let raw_url = url.as_z_str().context("url isn't string")?.to_str()?;
                let mut url = raw_url.to_string();

                if !url.contains("://") {
                    url.insert_str(0, "http://");
                }

                let url: Url = url.parse()?;
                if url.scheme() != "http" && url.scheme() != "https" {
                    return Ok(Box::new(()));
                }

                debug!("curl_getinfo get url: {}", &url);

                let host = match url.host_str() {
                    Some(host) => host,
                    None => return Ok(Box::new(())),
                };
                let port = match url.port() {
                    Some(port) => port,
                    None => match url.scheme() {
                        "http" => 80,
                        "https" => 443,
                        _ => 0,
                    },
                };
                let peer = &format!("{host}:{port}");

                let mut span = RequestContext::try_with_global_tracing_context(None, |ctx| {
                    ctx.create_exit_span(url.path(), peer)
                })?;

                span.with_span_object_mut(|span| {
                    span.component_id = COMPONENT_PHP_CURL_ID;
                    span.add_tag("url", raw_url);
                });

                let sw_header = RequestContext::try_with_global_tracing_context(None, |ctx| {
                    encode_propagation(ctx, url.path(), peer)
                })?;
                let mut val = CURL_HEADERS
                    .with(|headers| headers.borrow_mut().remove(&cid))
                    .unwrap_or_else(|| ZVal::from(ZArray::new()));
                if let Some(arr) = val.as_mut_z_arr() {
                    arr.insert(
                        InsertKey::NextIndex,
                        ZVal::from(format!("sw8: {}", sw_header)),
                    );
                    let ch = execute_data.get_parameter(0);
                    call(
                        "curl_setopt",
                        &mut [ch.clone(), ZVal::from(CURLOPT_HTTPHEADER), val],
                    )
                    .context("Call curl_setopt")?;
                }

                Ok(Box::new(span))
            }),
            Box::new(move |span, execute_data, _| {
                let mut span = span.downcast::<Span>().unwrap();

                let ch = execute_data.get_parameter(0);
                let result =
                    call("curl_getinfo", &mut [ch.clone()]).context("Call curl_get_info")?;
                let response = result.as_z_arr().context("response in not arr")?;
                let http_code = response
                    .get("http_code")
                    .and_then(|code| code.as_long())
                    .context("Call curl_getinfo, http_code is null")?;
                span.add_tag("status_code", &*http_code.to_string());
                if http_code == 0 {
                    let result =
                        call("curl_error", &mut [ch.clone()]).context("Call curl_get_info")?;
                    let curl_error = result
                        .as_z_str()
                        .context("curl_error is not string")?
                        .to_str()?;
                    span.with_span_object_mut(|span| {
                        span.is_error = true;
                        span.add_log(vec![("CURL_ERROR", curl_error)]);
                    });
                } else if http_code >= 400 {
                    span.with_span_object_mut(|span| span.is_error = true);
                } else {
                    span.with_span_object_mut(|span| span.is_error = false);
                }

                Ok(())
            }),
        )
    }

    #[tracing::instrument(skip_all)]
    fn execute_curl_close(&self) -> (Box<BeforeExecuteHook>, Box<AfterExecuteHook>) {
        (
            Box::new(|execute_data| {
                if execute_data.num_args() < 1 {
                    bail!("argument count incorrect");
                }

                let cid = Self::get_resource_id(execute_data)?;

                CURL_HEADERS.with(|headers| headers.borrow_mut().remove(&cid));

                Ok(Box::new(()))
            }),
            Box::new(|_, _, _| Ok(())),
        )
    }

    fn get_resource_id(execute_data: &mut ExecuteData) -> anyhow::Result<i64> {
        execute_data
            .get_parameter(0)
            .as_z_res()
            .map(|res| res.handle())
            .context("Get resource id failed")
    }
}
