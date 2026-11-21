use crate::ReportError;
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;

pub fn invalid() -> ReportError {
    ReportError::new("MODEL_RESPONSE_INVALID", "模型响应结构、内容范围或引用无效")
}
