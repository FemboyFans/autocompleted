use actix_web::{
    error, get,
    http::header,
    http::StatusCode,
    middleware::DefaultHeaders,
    web::{self, Data},
    HttpResponse, HttpResponseBuilder,
};
use deadpool_postgres::{Pool, Runtime};
use derive_more::{Display, Error, From};
use log::error;
use moka::future::Cache;
use serde::Deserialize;

mod config {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Config {
        pub server_addr: String,
        pub pg: deadpool_postgres::Config,
    }

    impl Config {
        pub fn from_env() -> Result<Self, config::ConfigError> {
            config::Config::builder()
                .add_source(config::Environment::default().separator("__"))
                .build()?
                .try_deserialize()
        }
    }
}

mod models {
    use serde::{Deserialize, Serialize};
    use tokio_pg_mapper_derive::PostgresMapper;

    #[derive(Deserialize, PostgresMapper, Serialize)]
    #[pg_mapper(table = "tags")] // singular 'user' is a keyword..
    pub struct Tag {
        pub id: i32,
        pub name: String,
        pub post_count: i32,
        pub category: i16,
        pub antecedent_name: Option<String>,
    }
}

mod db {
    use deadpool_postgres::Client;
    use tokio_pg_mapper::FromTokioPostgresRow;

    use crate::models::Tag;

    fn escape_like(stuff: &str) -> String {
        stuff
            .replace('%', "\\%")
            .replace('_', "\\_")
            .replace('*', "%")
            .replace("\\*", "*")
    }

    pub async fn get_tags(
        client: &Client,
        tag_prefix: &String,
    ) -> Result<Vec<Tag>, tokio_postgres::Error> {
        let escape_prefix = escape_like(&(tag_prefix.to_owned() + "*"));
        let _stmt = "set statement_timeout = 3000";
        let stmt = client.prepare(_stmt).await?;
        client.execute(&stmt, &[]).await?;
        let _stmt = include_str!("../sql/fetch_tags_a.sql");
        let stmt = client.prepare(_stmt).await?;
        let rows = client
            .query(&stmt, &[&escape_prefix])
            .await?
            .iter()
            .map(|row| Tag::from_row_ref(row).unwrap())
            .collect::<Vec<Tag>>();
        if !rows.is_empty() {
            return Ok(rows);
        }
        let _stmt = include_str!("../sql/fetch_tags_b.sql");
        let stmt = client.prepare(_stmt).await?;
        let rows = client
            .query(&stmt, &[&tag_prefix])
            .await?
            .iter()
            .map(|row| Tag::from_row_ref(row).unwrap())
            .collect::<Vec<Tag>>();
        Ok(rows)
    }
}

struct AutocompleteState {
    pool: Pool,
    cache: Cache<String, String>,
}

#[derive(Debug, Display, Error, From)]
enum AutocompleteError {
    #[display(fmt = "bad request")]
    BadRequest,
    #[display(fmt = "internal error")]
    ServerError,
}

impl error::ResponseError for AutocompleteError {
    fn error_response(&self) -> HttpResponse {
        match *self {
            AutocompleteError::BadRequest => HttpResponseBuilder::new(self.status_code())
                .insert_header((header::CONTENT_TYPE, "application/json; charset=utf-8"))
                .insert_header((header::CACHE_CONTROL, "private; max-age=0"))
                .body("{\"error\":\"bad request\"}"),
            AutocompleteError::ServerError => HttpResponseBuilder::new(self.status_code())
                .insert_header((header::CONTENT_TYPE, "application/json; charset=utf-8"))
                .insert_header((header::CACHE_CONTROL, "private; max-age=0"))
                .body("{\"error\":\"internal error\"}"),
        }
    }

    fn status_code(&self) -> StatusCode {
        match *self {
            AutocompleteError::BadRequest => StatusCode::BAD_REQUEST,
            AutocompleteError::ServerError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

fn validate_transform_tag(tag: &str) -> Result<String, AutocompleteError> {
    use unicode_normalization::UnicodeNormalization;
    if tag.len() > 100 {
        return Err(AutocompleteError::BadRequest);
    }
    if tag.len() < 3 {
        return Err(AutocompleteError::BadRequest);
    }
    let tag_str = tag
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .replace(['*', '%', '\0'], "")
        .chars()
        .filter(|x| !x.is_whitespace())
        .collect();
    Ok(tag_str)
}

#[derive(Deserialize)]
struct Req {
    #[serde(rename(deserialize = "search[name_matches]"))]
    tag_prefix: String,
}

#[get("/")]
async fn autocomplete(
    data: web::Data<AutocompleteState>,
    req: web::Query<Req>,
) -> Result<HttpResponse, AutocompleteError> {
    let prefix: String = validate_transform_tag(req.tag_prefix.as_str())?;
    let cached = data.cache.get(&prefix).await;
    return if let Some(cached_json) = cached {
        Ok(HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "application/json; charset=utf-8"))
            .insert_header((header::CACHE_CONTROL, "public, max-age=604800"))
            .body(cached_json))
    } else {
        let client = match data.pool.get().await {
            Ok(x) => x,
            Err(x) => {
                error!("{}", x.to_string());
                return Err(AutocompleteError::ServerError);
            }
        };
        let results = match db::get_tags(&client, &prefix).await {
            Ok(x) => x,
            Err(x) => {
                error!("{}", x.to_string());
                return Err(AutocompleteError::ServerError);
            }
        };
        let serialized = serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string());
        let serialized_copy = serialized.clone();
        data.cache.insert(prefix, serialized).await;
        Ok(HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "application/json; charset=utf-8"))
            .insert_header((header::CACHE_CONTROL, "public, max-age=604800"))
            .body(serialized_copy))
    };
}

#[get("/up")]
async fn healthcheck() -> HttpResponse {
    HttpResponse::NoContent().finish()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    use actix_web::{App, HttpServer};
    use moka::future::CacheBuilder;
    use std::time::Duration;
    use tokio_postgres::NoTls;
    env_logger::init();

    let config = crate::config::Config::from_env().unwrap();
    let pool = config.pg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap();
    let cache = CacheBuilder::new(15_000)
        .time_to_live(Duration::from_secs(6 * 60 * 60))
        .build();

    HttpServer::new(move || {
        App::new()
            .wrap(
                DefaultHeaders::new()
                    .add((header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"))
                    .add((header::ACCESS_CONTROL_ALLOW_HEADERS, "Authorization")),
            )
            .app_data(Data::new(AutocompleteState {
                pool: pool.clone(),
                cache: cache.clone(),
            }))
            .service(autocomplete)
            .service(healthcheck)
    })
    .bind(config.server_addr.clone())?
    .run()
    .await
}
