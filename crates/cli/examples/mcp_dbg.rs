use std::sync::Arc;
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let binary = faktor_test_mocks::mcp_bin();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let sup = faktor_terminal::ProcessSupervisor::new(cas);
    let cfg = faktor_mcp::McpConfig {
        name: "mock".into(),
        command: binary.to_string_lossy().into_owned(),
        args: vec![],
        env: vec![],
    };
    let r = faktor_mcp::McpServer::connect(cfg, sup).await;
    match r {
        Ok(s) => {
            println!("connect ok");
            let tools = s.list_tools().await.unwrap();
            println!(
                "tools: {:?}",
                tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
            );
            let r = s
                .call_tool(
                    "mock_echo",
                    serde_json::json!({"text": "hi"}),
                    std::time::Duration::from_secs(5),
                )
                .await;
            println!("call: {:?}", r.map(|x| x.content));
        }
        Err(e) => println!("connect err: {e}"),
    }
}
