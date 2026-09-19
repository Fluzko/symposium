//! Extension-invocation aggregate schema and supporting types.

mod bucket;
mod vocabulary;

pub(in crate::telemetry) use bucket::ExtensionInvocationAttribution;
pub(in crate::telemetry) use vocabulary::{
    ExtensionInvocationAgent, ExtensionInvocationPhase, ExtensionTargetScope,
    UnnamedExtensionReason,
};
