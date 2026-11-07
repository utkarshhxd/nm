// SPDX-License-Identifier: AGPL-3.0-only

use nm::{
    audit::{self, Actor, AuditEvent, actions},
    http::request_meta::RequestMeta,
};
use sqlx::{PgPool, Row};
use std::net::IpAddr;
use uuid::Uuid;

#[sqlx::test]
async fn record_writes_actor_and_ip(pool: PgPool) {
    let (u, w) = (Uuid::now_v7(), Uuid::now_v7());
    let meta = RequestMeta {
        ip: Some("2.2.2.2".parse().unwrap()),
        user_agent: Some("t".into()),
    };
    audit::record(
        &pool,
        &meta,
        AuditEvent {
            workspace_id: Some(w),
            actor: Actor::User(u),
            action: actions::WORKSPACE_CREATED,
            target_type: Some("workspace"),
            target_id: Some(w.to_string()),
            metadata: serde_json::json!({"name": "acme"}),
        },
    )
    .await
    .unwrap();

    let row = sqlx::query(
        "select workspace_id, actor_user_id, actor_api_key_id, action, target_type, target_id, \
         ip, user_agent, metadata from audit_log",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>("workspace_id"), Some(w));
    assert_eq!(row.get::<Option<Uuid>, _>("actor_user_id"), Some(u));
    assert_eq!(row.get::<Option<Uuid>, _>("actor_api_key_id"), None);
    assert_eq!(row.get::<String, _>("action"), "workspace.created");
    assert_eq!(
        row.get::<Option<String>, _>("target_type").as_deref(),
        Some("workspace")
    );
    assert_eq!(
        row.get::<Option<String>, _>("target_id"),
        Some(w.to_string())
    );
    assert_eq!(
        row.get::<Option<IpAddr>, _>("ip"),
        Some("2.2.2.2".parse().unwrap())
    );
    assert_eq!(
        row.get::<Option<String>, _>("user_agent").as_deref(),
        Some("t")
    );
    assert_eq!(
        row.get::<serde_json::Value, _>("metadata"),
        serde_json::json!({"name": "acme"})
    );
}

#[sqlx::test]
async fn api_key_and_system_actors_map_to_their_columns(pool: PgPool) {
    let k = Uuid::now_v7();
    let meta = RequestMeta {
        ip: None,
        user_agent: None,
    };
    for actor in [Actor::ApiKey(k), Actor::System] {
        audit::record(
            &pool,
            &meta,
            AuditEvent {
                workspace_id: None,
                actor,
                action: actions::API_KEY_CREATED,
                target_type: None,
                target_id: None,
                metadata: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
    }
    let rows = sqlx::query("select actor_user_id, actor_api_key_id, ip from audit_log order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows[0].get::<Option<Uuid>, _>("actor_api_key_id"), Some(k));
    assert_eq!(rows[0].get::<Option<Uuid>, _>("actor_user_id"), None);
    assert_eq!(rows[1].get::<Option<Uuid>, _>("actor_api_key_id"), None);
    assert_eq!(rows[1].get::<Option<Uuid>, _>("actor_user_id"), None);
    assert_eq!(rows[1].get::<Option<IpAddr>, _>("ip"), None);
}

#[sqlx::test]
async fn user_agent_is_truncated_to_512_chars(pool: PgPool) {
    // 'é' is two bytes: truncation must count characters, not bytes.
    let meta = RequestMeta {
        ip: None,
        user_agent: Some("é".repeat(600)),
    };
    audit::record(
        &pool,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::System,
            action: actions::AUTH_LOGOUT,
            target_type: None,
            target_id: None,
            metadata: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    let ua: String = sqlx::query_scalar("select user_agent from audit_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ua.chars().count(), 512);
}

#[sqlx::test]
async fn record_participates_in_the_callers_transaction(pool: PgPool) {
    let meta = RequestMeta {
        ip: None,
        user_agent: None,
    };
    let mut tx = pool.begin().await.unwrap();
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::System,
            action: actions::AUTH_LOGIN_FAILED,
            target_type: None,
            target_id: None,
            metadata: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from audit_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
