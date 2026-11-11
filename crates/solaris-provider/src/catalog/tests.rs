use super::*;
use solaris_backend::{Price, PriceTier, Usage};
use solaris_types::Support;

#[test]
fn bundled_snapshot_is_valid_and_offline_capabilities_are_provider_scoped() {
    let snapshot =
        CatalogSnapshot::from_json(include_str!("snapshot.json")).expect("valid snapshot");
    assert!(snapshot.providers.contains_key("openai"));
    assert!(
        snapshot
            .providers
            .keys()
            .all(|provider| SNAPSHOT_PROVIDERS.contains(&provider.as_str()))
    );
    assert!(!snapshot.providers.contains_key("openrouter"));
    let catalog = ModelCatalog::default();
    let official = catalog.resolve("anthropic", "claude-sonnet-4-5");
    assert_eq!(official.capabilities.context_window, Some(200_000));
    assert_eq!(official.capabilities.tool_calling, Support::Supported);
    assert!(official.price.is_some());
    let gateway = catalog.resolve("new-api", "claude-sonnet-4-5");
    assert_eq!(gateway.capabilities.tool_calling, Support::Supported);
    assert_eq!(gateway.price, official.price);
    assert_eq!(
        gateway.catalog.expect("official reference").provider,
        "anthropic"
    );
    assert!(
        catalog
            .resolve("anthropic", "claude-sonnet-4-5-imposter")
            .price
            .is_none()
    );
    assert!(
        catalog
            .resolve("anthropic", "CLAUDE-SONNET-4-5")
            .price
            .is_none()
    );
}

#[test]
fn alias_inherits_capabilities_but_never_another_providers_price() {
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
        "provider":"new-api","id":"alias","request_id":"routed-name",
        "catalog":{"provider":"anthropic","id":"claude-sonnet-4-5"},
        "capabilities":{"tool_calling":"unsupported","max_output":4096}
    }]}"#,
    )
    .expect("overrides");
    let resolved = catalog.resolve("new-api", "alias");
    assert_eq!(resolved.request_id, "routed-name");
    assert_eq!(resolved.capabilities.context_window, Some(200_000));
    assert_eq!(resolved.capabilities.max_output, Some(4096));
    assert_eq!(resolved.capabilities.tool_calling, Support::Unsupported);
    assert!(resolved.price.is_none());
    assert_eq!(resolved.sources["max_output"], "user");
}

#[test]
fn explicit_zero_prices_and_small_output_limits_survive_resolution() {
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
        "provider":"local","id":"free",
        "capabilities":{"context_window":1024,"max_output":128},
        "price":{"input":0,"output":0,"cache_read":0,"cache_write":0}
    }]}"#,
    )
    .expect("overrides");
    let model = catalog.resolve("local", "free");
    assert_eq!(model.price, Some(Price::FREE));
    assert_eq!(model.capabilities.price_known, Some(true));
    assert_eq!(
        super::output_budget(&model.capabilities, crate::DEFAULT_MAX_OUTPUT),
        128
    );
}

#[test]
fn models_dev_converts_tiers_and_does_not_invent_unknown_capabilities_or_prices() {
    let source = r#"{"host":{"models":{
      "test":{"type":"chat","modalities":{"input":["text","image/png"],"output":["text"]},
        "capabilities":{"tools":{"supported":false},"temperature":false},
        "limit":{"context":10000,"input":8000,"output":2000},
        "cost":{"text":{
          "input":{"price":2,"overrides":[{"when":{"context":{"gt":1000}},"price":4}]},
          "output":{"price":3,"overrides":[{"when":{"context":{"gt":1000}},"price":6}]}}}},
      "unknown":{"modalities":{"input":["text"],"output":["text"]}},
      "embedding":{"type":"embedding","modalities":{"output":["text"]}}
    }}}"#;
    let snapshot = parse_models_dev(source).expect("parse");
    let models = &snapshot.providers["host"];
    assert!(!models.contains_key("embedding"));
    let model = &models["test"];
    assert_eq!(model.capabilities.image_input, Support::Supported);
    assert_eq!(model.capabilities.temperature, Support::Unsupported);
    assert_eq!(model.capabilities.max_input, Some(8000));
    assert_eq!(model.capabilities.tool_calling, Support::Unsupported);
    assert_eq!(model.price_tiers.len(), 1);
    assert_eq!(model.price_tiers[0].price.output, 6.0);
    assert!(models["unknown"].price.is_none());
    assert_eq!(models["unknown"].capabilities.reasoning, Support::Unknown);
}

#[test]
fn openrouter_converts_per_token_prices_and_filters_non_chat_models() {
    let source = r#"{"data":[
      {"id":"vendor/test","architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},
        "supported_parameters":["tools","reasoning"],"context_length":32000,
        "top_provider":{"max_completion_tokens":1000},
        "pricing":{"prompt":"0.000002","completion":"0.000004","input_cache_read":"0.0000005"}},
      {"id":"image","architecture":{"output_modalities":["image"]}}
    ]}"#;
    let snapshot = parse_openrouter(source).expect("parse");
    let models = &snapshot.providers["openrouter"];
    assert_eq!(models.len(), 1);
    let model = &models["vendor/test"];
    assert_eq!(
        model.price.expect("price"),
        Price::per_million(2.0, 4.0, 0.5, 0.0)
    );
    assert_eq!(model.capabilities.reasoning, Support::Supported);
    assert_eq!(model.capabilities.image_input, Support::Supported);
}

#[test]
fn tier_rates_use_total_input_and_the_highest_matching_threshold() {
    let price = Price::per_million(1.0, 2.0, 0.1, 0.5);
    let tiers = [PriceTier {
        input_tokens_above: 1000,
        price: Price::per_million(3.0, 4.0, 0.2, 1.0),
    }];
    let usage = Usage::new(900, 20).with_cache(200, 0);
    assert_eq!(
        price.cost_with_tiers(&usage, &tiers),
        tiers[0].price.cost(&usage)
    );
    let boundary = Usage::new(1000, 20);
    assert_eq!(
        price.cost_with_tiers(&boundary, &tiers),
        price.cost(&boundary)
    );
}

#[test]
fn old_overrides_and_explicit_tier_configuration_remain_compatible() {
    let catalog = ModelCatalog::from_json(r#"{"models":[{
      "provider":"local","id":"test",
      "price":{"input":1,"output":2,"cache_read":0,"cache_write":0},
      "price_tiers":[{"input_tokens_above":1000,"input":3,"output":4,"cache_read":0,"cache_write":0}]
    }]}"#).expect("overrides with tier");
    let model = catalog.resolve("local", "test");
    assert_eq!(model.price_tiers.len(), 1);
}

#[tokio::test]
async fn request_alias_and_output_budget_reach_the_http_wire() {
    use crate::{AuthStore, BackendOptions, Credential, choose_backend, empty_environment};
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).await.expect("read");
            assert!(count > 0, "complete request");
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .expect("length");
                if request.len() >= end + 4 + length {
                    let body: serde_json::Value =
                        serde_json::from_slice(&request[end + 4..end + 4 + length]).expect("body");
                    let response = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    );
                    socket.write_all(header.as_bytes()).await.expect("header");
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("response");
                    break body;
                }
            }
        }
    });
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
      "provider":"custom","id":"friendly","request_id":"actual-model",
      "capabilities":{"max_output":1234},
      "price":{"input":1,"output":2,"cache_read":0,"cache_write":0}
    }]}"#,
    )
    .expect("catalog");
    let mut auth = AuthStore::new();
    auth.store(
        "custom",
        Credential::Endpoint {
            base_url: format!("http://{address}/v1"),
            api_key: "test".into(),
        },
    );
    auth.activate("custom");
    let choice = choose_backend(
        &auth,
        "friendly",
        BackendOptions {
            environment: empty_environment(),
            catalog: Arc::new(catalog),
            model_cache_dir: None,
        },
    );
    assert_eq!(choice.model, "friendly");
    let mut stream = choice
        .backend
        .run_turn(solaris_backend::TurnRequest::new(Vec::new(), "hello"))
        .await
        .expect("turn");
    let mut cost = None;
    while let Some(event) = stream.next().await {
        if let solaris_backend::AgentEvent::TurnComplete { cost_usd, .. } = event {
            cost = Some(cost_usd);
        }
    }
    let body = server.await.expect("server");
    assert_eq!(body["model"], "actual-model");
    assert_eq!(body["max_tokens"], 1234);
    assert!((cost.expect("cost") - 0.000014).abs() < 1e-10);
}

#[test]
fn requested_output_budget_is_separate_from_model_limits() {
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
      "provider":"custom","id":"test","output_tokens":12000,
      "capabilities":{"context_window":32000,"max_output":16000}
    }]}"#,
    )
    .expect("catalog");
    assert_eq!(catalog.resolve("custom", "test").output_tokens, 12000);
    assert_eq!(
        catalog
            .resolve("anthropic", "claude-sonnet-4-5")
            .output_tokens,
        8192
    );
}

#[test]
fn live_backends_adopt_new_metadata_without_mutating_an_inflight_snapshot() {
    use solaris_backend::{AgentBackend, Backend, Endpoint, TurnPlan, Wire};
    use std::sync::Arc;
    let catalog = Arc::new(ModelCatalog::default());
    let prototype = Backend::new(TurnPlan {
        endpoint: Endpoint {
            wire: Wire::OpenAiChat,
            session_header: None,
            base_url: "http://127.0.0.1:1/v1".into(),
            secret: None,
            name: "test",
            label: "test",
            env_keys: &[],
        },
        model: "test".into(),
        max_tokens: 8192,
        price: None,
        prompt_cache_key: None,
    })
    .expect("backend");
    let live = super::LiveModel {
        catalog,
        provider: "test",
        model: "test".into(),
        prototype,
        account_authorized: false,
    };
    let first = live.backend();
    assert_eq!(first.capabilities().context_window, Some(128000));
    let updated = CatalogSnapshot::from_json(
        r#"{"providers":{"test":{"test":{
        "name":"Test","capabilities":{"context_window":32000},"price":null
    }}}}"#,
    )
    .expect("updated snapshot");
    live.catalog
        .state
        .0
        .write()
        .expect("write")
        .insert("cache:models.dev".into(), updated);
    assert_eq!(live.backend().capabilities().context_window, Some(32000));
    assert_eq!(first.capabilities().context_window, Some(128000));
}

#[test]
fn gateway_fallback_preserves_user_overrides_and_request_identity() {
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
      "provider":"new-api","id":"gpt-5",
      "capabilities":{"max_output":1234},
      "price":{"input":0,"output":0,"cache_read":0,"cache_write":0}
    }]}"#,
    )
    .expect("catalog");
    let model = catalog.resolve("new-api", "gpt-5");
    assert_eq!(model.capabilities.tool_calling, Support::Supported);
    assert_eq!(model.capabilities.max_output, Some(1234));
    assert_eq!(model.request_id, "gpt-5");
    assert_eq!(model.price, Some(Price::FREE));
    assert_eq!(model.sources["price"], "user");
}

#[test]
fn providers_with_catalogs_and_ambiguous_ids_do_not_fall_back() {
    let catalog = ModelCatalog::default();
    assert!(catalog.resolve("anthropic", "gpt-5").catalog.is_none());
    let shared = CatalogSnapshot::from_json(
        r#"{"providers":{
      "openai":{"shared":{"name":"A","capabilities":{},"price":null}},
      "anthropic":{"shared":{"name":"B","capabilities":{},"price":null}}
    }}"#,
    )
    .expect("snapshot");
    catalog
        .state
        .0
        .write()
        .expect("write")
        .insert("cache:models.dev".into(), shared);
    assert!(catalog.resolve("new-api", "shared").catalog.is_none());
}

#[test]
fn small_models_have_a_safe_default_reply_budget() {
    let catalog = ModelCatalog::from_json(
        r#"{"models":[{
      "provider":"local","id":"small","capabilities":{"context_window":8192,"max_output":8192}}
    ]}"#,
    )
    .expect("catalog");
    let resolved = catalog.resolve("local", "small");
    assert_eq!(resolved.output_tokens, 2048);
    assert_eq!(resolved.capabilities.output_budget, Some(2048));
}
