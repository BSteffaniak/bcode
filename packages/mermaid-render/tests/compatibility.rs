use bcode_mermaid_render::{MermaidCancellationToken, MermaidRenderRequest};

#[test]
fn facade_preserves_worker_contract_type_identity() {
    let request: bmux_mermaid_render::MermaidRenderRequest =
        MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    let _: MermaidRenderRequest = request;
    let cancellation: bmux_mermaid_render::MermaidCancellationToken =
        MermaidCancellationToken::default();
    let _: MermaidCancellationToken = cancellation;
    assert_eq!(bcode_mermaid_render::RENDER_CONTRACT_VERSION, 1);
    let _: fn() = bcode_mermaid_render::worker_main;
}
