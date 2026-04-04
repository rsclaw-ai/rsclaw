use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

pub async fn node_pair_request(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::bad_request("node pairing not supported"))
}
pub async fn node_pair_approve(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::bad_request("node pairing not supported"))
}
pub async fn node_pair_reject(_ctx: MethodCtx) -> MethodResult {
    Err(ErrorShape::bad_request("node pairing not supported"))
}
