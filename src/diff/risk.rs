// SPDX-License-Identifier: MIT OR Apache-2.0

//! The risk table, in one place so it can be read as a table.
//!
//! Every rule the diff engine applies lives here as a fact → level mapping. The
//! analyzers produce facts; they do not decide risk themselves. That split is
//! what lets the policy be reviewed, documented and tested as a unit instead of
//! being scattered across `match` arms in the comparison code.
//!
//! Risk is static dependency-change risk: "what kind of change is this", not
//! "how likely is the agent to break". Nothing here estimates behavior —
//! behavioral probes are what answer that question, later.

use crate::config::RiskLevel;
use crate::manifest::DependencyKind;

/// Prompt facts (design spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptFact {
    /// Content differs, whitespace-collapsed shape does not.
    FormattingOnly,
    /// Both content and shape differ.
    TextChanged,
    Added,
    Removed,
}

pub fn prompt(fact: PromptFact) -> RiskLevel {
    match fact {
        // Phase 1 kept the content difference on purpose: text formatting can
        // still matter to a model. LOW, never NONE.
        PromptFact::FormattingOnly => RiskLevel::Low,
        // A substantive text change that Phase 2 cannot judge behaviorally.
        PromptFact::TextChanged => RiskLevel::Medium,
        PromptFact::Added => RiskLevel::Medium,
        // Removing a declared prompt may remove agent instructions entirely.
        PromptFact::Removed => RiskLevel::High,
    }
}

/// Model facts (design spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFact {
    Added,
    Removed,
    /// Ollama's SHA-256 of the model contents: the weights themselves changed.
    ContentDigestChanged,
    ProviderChanged,
    FamilyChanged,
    ParameterSizeChanged,
    QuantizationChanged,
    /// The openai-compatible endpoint, which is part of identity because that
    /// provider exposes no immutable digest.
    EndpointChanged,
    /// An identity subfield we can see changed but cannot name.
    IdentityOtherChanged,
    ParamsChanged,
    TemplateChanged,
    ToolsCapabilityAdded,
    ToolsCapabilityRemoved,
    CapabilityAdded,
    CapabilityRemoved,
    /// The capability facet changed but its payload does not say how.
    CapabilitiesChanged,
}

pub fn model(fact: ModelFact) -> RiskLevel {
    match fact {
        ModelFact::Added => RiskLevel::High,
        // A configured model dependency disappearing is a fundamental change.
        ModelFact::Removed => RiskLevel::Critical,
        ModelFact::ContentDigestChanged => RiskLevel::Critical,
        ModelFact::ProviderChanged => RiskLevel::Critical,
        ModelFact::FamilyChanged => RiskLevel::Critical,
        ModelFact::ParameterSizeChanged => RiskLevel::Critical,
        // Quantization changes behavior while keeping the nominal model id.
        ModelFact::QuantizationChanged => RiskLevel::High,
        // The deployment behind the endpoint may have changed. We say "endpoint
        // identity changed", never "the weights changed".
        ModelFact::EndpointChanged => RiskLevel::High,
        ModelFact::IdentityOtherChanged => RiskLevel::High,
        ModelFact::ParamsChanged => RiskLevel::Medium,
        // The template decides how messages reach the model.
        ModelFact::TemplateChanged => RiskLevel::High,
        // Gaining an execution-affecting capability surface.
        ModelFact::ToolsCapabilityAdded => RiskLevel::High,
        // An agent that relied on tool use may no longer support it.
        ModelFact::ToolsCapabilityRemoved => RiskLevel::Critical,
        ModelFact::CapabilityAdded => RiskLevel::Medium,
        ModelFact::CapabilityRemoved => RiskLevel::High,
        // Cannot tell which direction, so it keeps the stronger of the two.
        ModelFact::CapabilitiesChanged => RiskLevel::High,
    }
}

/// Tool facts (design spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFact {
    Added,
    Removed,
    DescriptionChanged,
    /// Description content differs, shape does not.
    DescriptionFormattingOnly,
    CapabilityAdded,
    CapabilityRemoved,
}

pub fn tool(fact: ToolFact) -> RiskLevel {
    match fact {
        ToolFact::Added => RiskLevel::High,
        ToolFact::Removed => RiskLevel::High,
        // A description is model input, not documentation.
        ToolFact::DescriptionChanged => RiskLevel::Medium,
        ToolFact::DescriptionFormattingOnly => RiskLevel::Low,
        ToolFact::CapabilityAdded => RiskLevel::Medium,
        // Permission semantics are not modelled yet, so a dangerous addition is
        // not CRITICAL here — but losing a capability stays HIGH.
        ToolFact::CapabilityRemoved => RiskLevel::High,
    }
}

/// Which side of a tool contract a schema belongs to.
///
/// The same structural change carries different risk per side: an input schema
/// decides whether the agent's calls are still valid, an output schema decides
/// whether the caller can still parse what comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaSide {
    Input,
    Output,
}

/// Structural facts the bounded schema analyzer can name (design spec §8.2, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaFact {
    RequiredAdded,
    RequiredRemoved,
    PropertyRemoved,
    OptionalPropertyAdded,
    TypeChanged,
    EnumNarrowed,
    EnumExpanded,
    /// A `description` anywhere in the schema — at a property, an item, or the
    /// schema itself. Descriptions are model input, so this is a behavior change
    /// and not documentation noise.
    DescriptionChanged,
    /// `additionalProperties` stopped admitting anything it used to.
    AdditionalPropertiesTightened,
    /// `additionalProperties` started admitting more than it used to.
    AdditionalPropertiesLoosened,
    /// The digests differ but the analyzer could not classify the difference.
    Generic,
}

pub fn schema(side: SchemaSide, fact: SchemaFact) -> RiskLevel {
    match (side, fact) {
        // A newly required input can make previously valid calls invalid.
        (SchemaSide::Input, SchemaFact::RequiredAdded) => RiskLevel::Critical,
        // Input required.  Removing the marker makes the contract more permissive.
        (SchemaSide::Input, SchemaFact::RequiredRemoved) => RiskLevel::Medium,
        (SchemaSide::Input, SchemaFact::PropertyRemoved) => RiskLevel::High,
        (SchemaSide::Input, SchemaFact::OptionalPropertyAdded) => RiskLevel::Low,
        (SchemaSide::Input, SchemaFact::TypeChanged) => RiskLevel::Critical,
        (SchemaSide::Input, SchemaFact::EnumNarrowed) => RiskLevel::High,
        (SchemaSide::Input, SchemaFact::EnumExpanded) => RiskLevel::Medium,

        // Downstream parsing is what an output schema protects: a field the
        // caller relied on disappearing is the breaking direction.
        (SchemaSide::Output, SchemaFact::RequiredRemoved) => RiskLevel::Critical,
        (SchemaSide::Output, SchemaFact::RequiredAdded) => RiskLevel::Medium,
        (SchemaSide::Output, SchemaFact::PropertyRemoved) => RiskLevel::High,
        (SchemaSide::Output, SchemaFact::OptionalPropertyAdded) => RiskLevel::Low,
        (SchemaSide::Output, SchemaFact::TypeChanged) => RiskLevel::High,
        (SchemaSide::Output, SchemaFact::EnumNarrowed) => RiskLevel::High,
        (SchemaSide::Output, SchemaFact::EnumExpanded) => RiskLevel::Medium,

        // Guidance the model reads is guidance it can stop following: a property
        // description is the difference between a valid call and a rejected one.
        (_, SchemaFact::DescriptionChanged) => RiskLevel::Medium,

        // Refusing properties that used to be accepted can invalidate calls that
        // worked; accepting more can only widen what the tool must handle.
        (_, SchemaFact::AdditionalPropertiesTightened) => RiskLevel::High,
        (_, SchemaFact::AdditionalPropertiesLoosened) => RiskLevel::Medium,

        // Fail-safe floor: an unclassified schema change is reported as a
        // potentially breaking contract change rather than quietly downgraded.
        (_, SchemaFact::Generic) => RiskLevel::High,
    }
}

/// MCP server facts (design spec §8.3). Discovery itself is a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFact {
    Added,
    Removed,
    /// Era, protocol version, or supported versions changed.
    ProtocolChanged,
    /// Server implementation name or version changed.
    ServerInfoChanged,
    /// Identity changed but the payload does not say which part.
    IdentityOtherChanged,
}

pub fn mcp(fact: McpFact) -> RiskLevel {
    match fact {
        McpFact::Added => RiskLevel::High,
        McpFact::Removed => RiskLevel::High,
        McpFact::ProtocolChanged => RiskLevel::High,
        McpFact::ServerInfoChanged => RiskLevel::Medium,
        McpFact::IdentityOtherChanged => RiskLevel::High,
    }
}

/// Facets the bounded schema analyzer interprets.
pub fn is_schema_facet(name: &str) -> bool {
    matches!(name, "input_schema" | "output_schema")
}

/// A dependency whose id is unchanged but whose kind is not.
///
/// That cannot come from discovery; it means a hand-edited or future-format
/// lockfile, and continuing as though only facets changed would be a silent
/// reinterpretation of state we do not understand.
pub fn kind_mismatch() -> RiskLevel {
    RiskLevel::Critical
}

/// A dependency that exists only in the current state.
pub fn added(kind: DependencyKind) -> RiskLevel {
    match kind {
        DependencyKind::Model => model(ModelFact::Added),
        DependencyKind::Prompt => prompt(PromptFact::Added),
        DependencyKind::Tool => tool(ToolFact::Added),
        DependencyKind::McpServer => mcp(McpFact::Added),
    }
}

/// A dependency that exists only in the baseline.
pub fn removed(kind: DependencyKind) -> RiskLevel {
    match kind {
        DependencyKind::Model => model(ModelFact::Removed),
        DependencyKind::Prompt => prompt(PromptFact::Removed),
        DependencyKind::Tool => tool(ToolFact::Removed),
        DependencyKind::McpServer => mcp(McpFact::Removed),
    }
}

/// A facet that exists only in the current state.
pub fn facet_added(_kind: DependencyKind, name: &str) -> RiskLevel {
    if is_schema_facet(name) {
        // A tool that gained an input or output schema became a different
        // contract, not merely a better-described one.
        return RiskLevel::High;
    }
    RiskLevel::Medium
}

/// A facet that exists only in the baseline.
pub fn facet_removed(_kind: DependencyKind, _name: &str) -> RiskLevel {
    // Losing fingerprint surface is the more dangerous direction: we can no
    // longer see changes in whatever stopped being reported.
    RiskLevel::High
}

/// A facet whose digest changed but which no analyzer could explain.
///
/// The invariant this exists to protect: a real digest difference is never
/// reported as no change. Over-reporting is the acceptable error here.
pub fn unknown_facet(_kind: DependencyKind, name: &str) -> RiskLevel {
    if is_schema_facet(name) {
        return RiskLevel::High;
    }
    RiskLevel::Medium
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests are the policy. If one of these rules were quietly changed,
    // the corresponding assertion fails — which is the point of keeping the
    // table in one testable place.

    #[test]
    fn prompt_rules() {
        assert_eq!(prompt(PromptFact::FormattingOnly), RiskLevel::Low);
        assert_eq!(prompt(PromptFact::TextChanged), RiskLevel::Medium);
        assert_eq!(prompt(PromptFact::Added), RiskLevel::Medium);
        assert_eq!(prompt(PromptFact::Removed), RiskLevel::High);
    }

    #[test]
    fn formatting_only_is_low_and_never_none() {
        // Phase 1 kept the content difference deliberately, so the diff layer
        // must not call it "nothing happened".
        assert_ne!(prompt(PromptFact::FormattingOnly), RiskLevel::None);
    }

    #[test]
    fn model_rules() {
        assert_eq!(model(ModelFact::Added), RiskLevel::High);
        assert_eq!(model(ModelFact::Removed), RiskLevel::Critical);

        assert_eq!(model(ModelFact::ContentDigestChanged), RiskLevel::Critical);
        assert_eq!(model(ModelFact::ProviderChanged), RiskLevel::Critical);
        assert_eq!(model(ModelFact::FamilyChanged), RiskLevel::Critical);
        assert_eq!(model(ModelFact::ParameterSizeChanged), RiskLevel::Critical);

        assert_eq!(model(ModelFact::QuantizationChanged), RiskLevel::High);
        assert_eq!(model(ModelFact::EndpointChanged), RiskLevel::High);
        assert_eq!(model(ModelFact::IdentityOtherChanged), RiskLevel::High);
        assert_eq!(model(ModelFact::TemplateChanged), RiskLevel::High);

        assert_eq!(model(ModelFact::ParamsChanged), RiskLevel::Medium);

        assert_eq!(model(ModelFact::ToolsCapabilityAdded), RiskLevel::High);
        assert_eq!(
            model(ModelFact::ToolsCapabilityRemoved),
            RiskLevel::Critical
        );
        assert_eq!(model(ModelFact::CapabilityAdded), RiskLevel::Medium);
        assert_eq!(model(ModelFact::CapabilityRemoved), RiskLevel::High);
    }

    #[test]
    fn tool_rules() {
        assert_eq!(tool(ToolFact::Added), RiskLevel::High);
        assert_eq!(tool(ToolFact::Removed), RiskLevel::High);

        assert_eq!(tool(ToolFact::DescriptionChanged), RiskLevel::Medium);
        assert_eq!(tool(ToolFact::DescriptionFormattingOnly), RiskLevel::Low);

        assert_eq!(tool(ToolFact::CapabilityAdded), RiskLevel::Medium);
        assert_eq!(tool(ToolFact::CapabilityRemoved), RiskLevel::High);
    }

    #[test]
    fn schema_rules_differ_by_side() {
        // The input side decides whether the agent's calls stay valid.
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::RequiredAdded),
            RiskLevel::Critical
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::RequiredRemoved),
            RiskLevel::Medium
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::PropertyRemoved),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::OptionalPropertyAdded),
            RiskLevel::Low
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::TypeChanged),
            RiskLevel::Critical
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::EnumNarrowed),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::EnumExpanded),
            RiskLevel::Medium
        );

        // The output side protects downstream parsing instead.
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::RequiredRemoved),
            RiskLevel::Critical
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::RequiredAdded),
            RiskLevel::Medium
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::PropertyRemoved),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::OptionalPropertyAdded),
            RiskLevel::Low
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::TypeChanged),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::EnumNarrowed),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::EnumExpanded),
            RiskLevel::Medium
        );

        // A difference the analyzer could not name is never quiet, on either side.
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::Generic),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::Generic),
            RiskLevel::High
        );
    }

    #[test]
    fn schema_description_and_additional_properties_rows() {
        // Spec §8.3: a property description is MEDIUM, because the guidance the
        // model reads is guidance it can stop following.
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::DescriptionChanged),
            RiskLevel::Medium
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::DescriptionChanged),
            RiskLevel::Medium
        );

        // Tightening refuses arguments that used to be accepted; loosening only
        // widens what the tool must handle.
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::AdditionalPropertiesTightened),
            RiskLevel::High
        );
        assert_eq!(
            schema(
                SchemaSide::Output,
                SchemaFact::AdditionalPropertiesTightened
            ),
            RiskLevel::High
        );
        assert_eq!(
            schema(SchemaSide::Input, SchemaFact::AdditionalPropertiesLoosened),
            RiskLevel::Medium
        );
        assert_eq!(
            schema(SchemaSide::Output, SchemaFact::AdditionalPropertiesLoosened),
            RiskLevel::Medium
        );
    }

    #[test]
    fn mcp_rules() {
        assert_eq!(mcp(McpFact::Added), RiskLevel::High);
        assert_eq!(mcp(McpFact::Removed), RiskLevel::High);
        assert_eq!(mcp(McpFact::ProtocolChanged), RiskLevel::High);
        assert_eq!(mcp(McpFact::ServerInfoChanged), RiskLevel::Medium);
        assert_eq!(mcp(McpFact::IdentityOtherChanged), RiskLevel::High);
    }

    #[test]
    fn a_kind_mismatch_is_critical() {
        assert_eq!(kind_mismatch(), RiskLevel::Critical);
    }

    #[test]
    fn added_and_removed_risks_follow_the_per_kind_rules() {
        assert_eq!(added(DependencyKind::Model), RiskLevel::High);
        assert_eq!(removed(DependencyKind::Model), RiskLevel::Critical);
        assert_eq!(added(DependencyKind::Prompt), RiskLevel::Medium);
        assert_eq!(removed(DependencyKind::Prompt), RiskLevel::High);
        assert_eq!(added(DependencyKind::Tool), RiskLevel::High);
        assert_eq!(removed(DependencyKind::Tool), RiskLevel::High);
        assert_eq!(added(DependencyKind::McpServer), RiskLevel::High);
        assert_eq!(removed(DependencyKind::McpServer), RiskLevel::High);

        // Losing a declared model is the one whole-dependency change that is
        // CRITICAL on its own.
        assert_eq!(removed(DependencyKind::Model), RiskLevel::Critical);
    }

    #[test]
    fn capabilities_changed_is_never_quiet() {
        assert_eq!(model(ModelFact::CapabilitiesChanged), RiskLevel::High);
    }

    #[test]
    fn losing_a_facet_is_never_less_than_gaining_one() {
        assert_eq!(
            facet_removed(DependencyKind::Model, "template"),
            RiskLevel::High
        );
        assert_eq!(
            facet_added(DependencyKind::Model, "params"),
            RiskLevel::Medium
        );
        assert!(
            facet_removed(DependencyKind::Model, "params")
                >= facet_added(DependencyKind::Model, "params")
        );

        // A schema appearing is a contract change, not a documentation change.
        assert_eq!(
            facet_added(DependencyKind::Tool, "input_schema"),
            RiskLevel::High
        );
    }

    #[test]
    fn an_unexplained_change_is_never_quiet() {
        // The invariant the whole phase rests on: a different digest must never
        // come out as "nothing happened", whatever the facet or kind is.
        let kinds = [
            DependencyKind::Model,
            DependencyKind::Prompt,
            DependencyKind::Tool,
            DependencyKind::McpServer,
        ];
        let names = [
            "identity",
            "content",
            "shape",
            "params",
            "template",
            "capabilities",
            "input_schema",
            "output_schema",
            "description",
            "something_from_a_future_version",
        ];

        for kind in kinds {
            for name in names {
                assert!(
                    unknown_facet(kind, name) >= RiskLevel::Medium,
                    "unknown change to {kind:?}/{name} fell below MEDIUM"
                );
                assert!(
                    facet_added(kind, name) >= RiskLevel::Medium,
                    "added facet {kind:?}/{name} fell below MEDIUM"
                );
                assert!(
                    facet_removed(kind, name) >= RiskLevel::Medium,
                    "removed facet {kind:?}/{name} fell below MEDIUM"
                );
            }
        }
    }
}
