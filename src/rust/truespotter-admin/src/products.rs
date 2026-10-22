//! GET /api/products — port of the ProductEndpoints.cs LIST handler →
//! ListProductsQuery → OpenSearchProductRepository.ListAsync (the catalog
//! lives in OpenSearch, NOT the Postgres products table). Wire shape:
//! ListProductsResult { items, total, brands, categories } with ProductDto
//! items (camelCase) mapped from the OS docs exactly like MapDocToProduct +
//! ProductMapper.ToDto.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::{AppState, DotNetTime};

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    pub q: Option<String>,
    pub brand: Option<String>,
    pub category: Option<String>,
    pub sort_by: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// FacetBucket (IProductRepository.cs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetBucket {
    pub key: String,
    pub count: i64,
}

/// OfferDto (ProductDto.cs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferDto {
    pub id: String,
    pub shop_name: String,
    pub price: f64,
    pub currency: String,
    pub url: Option<String>,
    pub in_stock: bool,
}

/// CommentDto (ProductDto.cs). Source is the .NET enum ToString:
/// "Admin" / "External".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentDto {
    pub id: String,
    pub author: String,
    pub body: String,
    pub source: String,
    pub created_at: DotNetTime,
}

/// ProductDto (ProductDto.cs) — camelCase wire order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductDto {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub vendor: String,
    pub category: String,
    pub tags: Vec<String>,
    pub image_url: Option<String>,
    pub external_ref: Option<String>,
    pub created_at: DotNetTime,
    pub updated_at: DotNetTime,
    pub offers: Vec<OfferDto>,
    pub comments: Vec<CommentDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProductsResult {
    pub items: Vec<ProductDto>,
    pub total: i64,
    pub brands: Vec<FacetBucket>,
    pub categories: Vec<FacetBucket>,
}

fn s(source: &Value, key: &str) -> String {
    source
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn opt_s(source: &Value, key: &str) -> Option<String> {
    source
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn parse_dt(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            // OpenSearch stores epoch millis for date fields; docs indexed
            // by hand may carry ISO strings (handled above).
            v.as_i64().and_then(DateTime::from_timestamp_millis)
        })
}

/// MapDocToProduct + ProductMapper.ToDto port. Id is a fresh GUID per read
/// in .NET too (Product.Create never restores Id) — any GUID is wire-valid;
/// we parse the doc "id" when it is a GUID, else generate one.
fn map_doc_to_dto(id: &str, source: &Value) -> ProductDto {
    let slug = opt_s(source, "slug").unwrap_or_else(|| id.to_string());
    let vendor = s(source, "vendor");
    let external_ref = opt_s(source, "id");

    let mut offers = Vec::new();
    if let Some(offers_el) = source.get("offers").filter(|o| o.as_array().is_some()) {
        for offer in offers_el.as_array().unwrap() {
            let shop_name = offer
                .get("shop_name")
                .or_else(|| offer.get("shopName"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let price = offer.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let currency = offer
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("PLN");
            let url = offer
                .get("checkout_url")
                .or_else(|| offer.get("url"))
                .and_then(|v| v.as_str());
            let in_stock = offer
                .get("in_stock")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !shop_name.trim().is_empty() && price > 0.0 {
                offers.push(OfferDto {
                    id: offer
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|v| uuid::Uuid::parse_str(v).is_ok())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    shop_name: shop_name.to_string(),
                    price,
                    currency: currency.to_string(),
                    url: url.map(|u| u.to_string()),
                    in_stock,
                });
            }
        }
    } else if let Some(flat) = source
        .get("price")
        .and_then(|v| v.as_f64())
        .filter(|p| *p > 0.0)
    {
        // Legacy flat-price docs → synthesize one offer (.NET parity).
        let currency = source
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("PLN");
        let url = opt_s(source, "product_url");
        offers.push(OfferDto {
            id: uuid::Uuid::new_v4().to_string(),
            shop_name: vendor.clone(),
            price: flat,
            currency: currency.to_string(),
            url,
            in_stock: true,
        });
    }

    let mut comments = Vec::new();
    if let Some(arr) = source.get("comments").and_then(|c| c.as_array()) {
        for c in arr {
            let author = c.get("author").and_then(|v| v.as_str()).unwrap_or("");
            let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
            if author.trim().is_empty() || body.trim().is_empty() {
                continue;
            }
            comments.push(CommentDto {
                id: c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|v| uuid::Uuid::parse_str(v).is_ok())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                author: author.to_string(),
                body: body.to_string(),
                source:
                    if c.get("source").and_then(|v| v.as_str()).unwrap_or("admin") == "external" {
                        "External"
                    } else {
                        "Admin"
                    }
                    .to_string(),
                created_at: DotNetTime(
                    parse_dt(c.get("created_at").unwrap_or(&Value::Null)).unwrap_or_else(Utc::now),
                ),
            });
        }
    }

    ProductDto {
        id: uuid::Uuid::parse_str(external_ref.clone().unwrap_or_default().as_str())
            .map(|u| u.to_string())
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        slug,
        title: s(source, "title"),
        description: s(source, "description"),
        vendor,
        category: s(source, "type"),
        tags: source
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        image_url: opt_s(source, "image_url"),
        external_ref,
        created_at: DotNetTime(
            source
                .get("created_at")
                .and_then(parse_dt)
                .unwrap_or_else(Utc::now),
        ),
        updated_at: DotNetTime(Utc::now()),
        offers,
        comments,
    }
}

/// OpenSearchProductRepository.ListAsync — the exact request body.
fn build_search_body(params: &ListParams, page: i64, page_size: i64) -> Value {
    let from = (page - 1) * page_size;
    let sort_key = match params.sort_by.as_deref().unwrap_or_default() {
        "slug" => "slug.keyword",
        "title" => "title.keyword",
        "vendor" => "vendor.keyword",
        "price" => "price",
        _ => "created_at",
    };
    let order = match params.sort_by.as_deref().unwrap_or_default() {
        "" | "created" => "desc",
        _ => "asc",
    };

    let mut must = vec![];
    match params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => must.push(json!({ "multi_match": { "query": q,
            "fields": ["title^3", "vendor^2", "description", "tags", "specs_text"] } })),
        None => must.push(json!({ "match_all": {} })),
    }
    let mut filter = vec![];
    if let Some(brand) = params
        .brand
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        filter.push(json!({ "term": { "vendor.keyword": brand } }));
    }
    if let Some(category) = params
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        filter.push(json!({ "term": { "type.keyword": category } }));
    }

    json!({
        "size": page_size,
        "from": from,
        "track_total_hits": true,
        "_source": { "excludes": ["embedding"] },
        "query": { "bool": { "must": must, "filter": filter } },
        "sort": [{ sort_key: { "order": order } }],
        "aggs": {
            "brands": { "terms": { "field": "vendor.keyword", "size": 100 } },
            "categories": { "terms": { "field": "type.keyword", "size": 100 } },
        },
    })
}

async fn list_products(
    state: &AppState,
    params: &ListParams,
    page: i64,
    page_size: i64,
) -> Result<ListProductsResult, reqwest::Error> {
    let url = format!(
        "{}/{}/_search",
        state.opensearch_url.trim_end_matches('/'),
        state.opensearch_index
    );
    let mut req = state.http_client.post(&url);
    if let (Some(user), Some(password)) = (&state.opensearch_user, &state.opensearch_password) {
        req = req.basic_auth(user, Some(password));
    }
    let data: Value = req
        .json(&build_search_body(params, page, page_size))
        .send()
        .await?
        .json()
        .await?;

    let hits = &data["hits"]["hits"];
    let total = data["hits"]["total"]["value"].as_i64().unwrap_or(0);
    let items = hits
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|h| {
                    map_doc_to_dto(
                        h.get("_id").and_then(|v| v.as_str()).unwrap_or(""),
                        &h["_source"],
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    fn buckets(aggs: &Value, name: &str) -> Vec<FacetBucket> {
        aggs[name]["buckets"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|b| FacetBucket {
                        key: b["key"].as_str().unwrap_or_default().to_string(),
                        count: b["doc_count"].as_i64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    let aggs = &data["aggregations"];

    Ok(ListProductsResult {
        items,
        total,
        brands: buckets(aggs, "brands"),
        categories: buckets(aggs, "categories"),
    })
}

pub async fn list_products_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
) -> Response {
    let page = params.page.unwrap_or(1).max(1);
    let page_size = params.page_size.unwrap_or(50).max(1);
    // .NET: a failed OpenSearch call → (empty, 0, [], []), still 200.
    match list_products(&state, &params, page, page_size).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "OpenSearch product list failed — returning empty result");
            Json(ListProductsResult {
                items: vec![],
                total: 0,
                brands: vec![],
                categories: vec![],
            })
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_keys_match_adminservice_adminproduct() {
        // AdminService.ts AdminProduct consumes: id, slug, title, vendor,
        // category, createdAt, updatedAt (subset of the full ProductDto).
        let dto = ProductDto {
            id: "id".into(),
            slug: "slug".into(),
            title: "t".into(),
            description: "d".into(),
            vendor: "v".into(),
            category: "c".into(),
            tags: vec![],
            image_url: None,
            external_ref: None,
            created_at: DotNetTime::default(),
            updated_at: DotNetTime::default(),
            offers: vec![],
            comments: vec![],
        };
        let json = serde_json::to_value(&dto).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "slug",
                "title",
                "description",
                "vendor",
                "category",
                "tags",
                "imageUrl",
                "externalRef",
                "createdAt",
                "updatedAt",
                "offers",
                "comments",
            ]
        );
    }

    #[test]
    fn search_body_defaults_match_dotnet() {
        let params = ListParams {
            q: None,
            brand: None,
            category: None,
            sort_by: None,
            page: None,
            page_size: None,
        };
        let body = build_search_body(&params, 1, 20);
        assert_eq!(body["size"], 20);
        assert_eq!(body["from"], 0);
        assert_eq!(body["track_total_hits"], true);
        assert_eq!(body["query"]["bool"]["must"][0], json!({ "match_all": {} }));
        assert_eq!(body["sort"][0]["created_at"]["order"], "desc");
        assert_eq!(body["aggs"]["brands"]["terms"]["field"], "vendor.keyword");
    }

    #[test]
    fn search_body_query_and_sort_variants() {
        for (sort_by, key, order) in [
            ("slug", "slug.keyword", "asc"),
            ("title", "title.keyword", "asc"),
            ("vendor", "vendor.keyword", "asc"),
            ("price", "price", "asc"),
            ("created", "created_at", "desc"),
        ] {
            let params = ListParams {
                q: Some("buty".into()),
                brand: Some("Nike".into()),
                category: Some("Obuwie".into()),
                sort_by: Some(sort_by.into()),
                page: Some(2),
                page_size: Some(10),
            };
            let body = build_search_body(&params, 2, 10);
            assert_eq!(body["from"], 10);
            assert_eq!(
                body["query"]["bool"]["must"][0]["multi_match"]["query"],
                "buty"
            );
            assert_eq!(
                body["query"]["bool"]["filter"][0]["term"]["vendor.keyword"],
                "Nike"
            );
            assert_eq!(
                body["query"]["bool"]["filter"][1]["term"]["type.keyword"],
                "Obuwie"
            );
            assert_eq!(body["sort"][0][key]["order"], order);
        }
    }

    #[test]
    fn map_doc_matches_dotnet_shapes() {
        let doc = json!({
            "id": "ext-123",
            "slug": "nike-air-max",
            "title": "Nike Air Max",
            "vendor": "Nike",
            "type": "Obuwie",
            "description": "desc",
            "tags": ["running"],
            "image_url": "https://x/img.png",
            "created_at": "2026-01-02T03:04:05Z",
            "offers": [
                { "id": "11111111-1111-1111-1111-111111111111", "shop_name": "Sklep",
                  "price": 199.99, "currency": "PLN", "checkout_url": "https://s/1", "in_stock": true }
            ],
            "comments": [
                { "id": "22222222-2222-2222-2222-222222222222", "author": "Ala",
                  "body": "dobre", "source": "external", "created_at": "2026-02-03T00:00:00Z" }
            ]
        });
        let dto = map_doc_to_dto("nike-air-max", &doc);
        assert_eq!(dto.slug, "nike-air-max");
        assert_eq!(dto.vendor, "Nike");
        assert_eq!(dto.category, "Obuwie");
        assert_eq!(dto.external_ref.as_deref(), Some("ext-123"));
        assert_eq!(dto.offers.len(), 1);
        assert_eq!(dto.offers[0].shop_name, "Sklep");
        assert_eq!(dto.offers[0].price, 199.99);
        assert_eq!(dto.comments[0].source, "External");
        assert!(dto.id.chars().count() == 36); // fresh GUID (ext-123 not a GUID)
    }

    #[test]
    fn legacy_flat_price_synthesizes_one_offer() {
        let doc = json!({ "slug": "x", "vendor": "V", "price": 49.9, "currency": "PLN", "product_url": "https://s/x" });
        let dto = map_doc_to_dto("x", &doc);
        assert_eq!(dto.offers.len(), 1);
        assert_eq!(dto.offers[0].price, 49.9);
        assert_eq!(dto.offers[0].shop_name, "V");
    }
}
