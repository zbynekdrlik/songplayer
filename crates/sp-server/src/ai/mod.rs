//! Text AI (#321): every text task's chat completion through OpenRouter.

pub mod client;
pub mod openrouter; // #321: the text-LLM client over OpenRouter's chat API
pub mod openrouter_probe; // #321: the OpenRouter path's live gate
pub mod retry; // #145: the waits of a refused call
#[cfg(test)]
pub mod test_support;
pub mod translate_ab; // #321: the blind A/B of the translation
