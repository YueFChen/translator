use wonderland_translator::*;

const HEADER: &str =
    "// Generated from Rust by examples/generate_bindings.rs. Do not edit.\n";

fn main() {
    let cfg = Config::default();
    let mut out = String::from(HEADER);
    macro_rules! emit { ($($t:ty),*) => { $(out.push_str("export "); out.push_str(&<$t>::decl(&cfg)); out.push('\n');)* }; }
    emit!(
        Locale,
        SourceVersion,
        ColumnMapping,
        TargetColumn,
        Segment,
        PlaceholderSignature,
        TranslationUnit,
        CellState,
        IssueSeverity,
        QualityIssue,
        JobStatus,
        ProviderCapabilities,
        LocalePair,
        CsvInspection,
        TargetSummary,
        Preflight,
        TranslationExport,
        MemoryEntry,
        TermKind,
        GlossaryTerm,
        TermInput,
        TermHit,
        ProviderConfig,
        ProviderConfigInput,
        JobLimits,
        JobProgress,
        WorkRecord,
        WorkJob,
        WorkTree,
        CsvPage,
        ProviderProfile
    );
    let out = format!(
        "{}\n",
        out.lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("ui/src/types.generated.ts");
    if std::env::args().any(|arg| arg == "--check") {
        assert_eq!(
            std::fs::read_to_string(path).expect("generated file"),
            out,
            "Bindings drift: regenerate bindings"
        );
    } else {
        std::fs::write(path, out).unwrap();
    }
}
