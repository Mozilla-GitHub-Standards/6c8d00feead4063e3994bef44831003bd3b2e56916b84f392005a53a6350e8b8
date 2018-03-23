use std::convert::Into;
use std::io::Read;
use std::error::Error;

use diesel::{QueryDsl, RunQueryDsl};
use diesel::dsl::sql;
use diesel::result::Error as DieselError;
use diesel::sql_types::Integer;
use failure::ResultExt;
use rocket::{self, Data, Request, Rocket};
use rocket::data::{self, FromData};
use rocket::http::Status;
use rocket::Outcome::{Failure, Success};
use rocket::outcome::IntoOutcome;
use rocket::request::{self, FromRequest};
use rocket::response::{content, status};
use rocket_contrib::Json;

use auth;
use db;
use db::models::{Broadcaster, Reader};
use db::schema::broadcastsv1;
use error::{HandlerError, HandlerErrorKind, HandlerResult, Result, VALIDATION_FAILED};

impl<'a, 'r> FromRequest<'a, 'r> for Broadcaster {
    type Error = HandlerError;

    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, HandlerError> {
        auth::authorized_broadcaster(request).into_outcome(VALIDATION_FAILED)
    }
}

/// A new broadcast
struct VersionInput {
    value: String,
}

impl FromData for VersionInput {
    type Error = HandlerError;

    fn from_data(_: &Request, data: Data) -> data::Outcome<Self, HandlerError> {
        let mut string = String::new();
        data.open()
            .read_to_string(&mut string)
            .context(HandlerErrorKind::MissingVersionDataError)
            .map_err(Into::into)
            .into_outcome(VALIDATION_FAILED)?;
        if string.is_empty() {
            return Failure((
                VALIDATION_FAILED,
                HandlerErrorKind::InvalidVersionDataError.into(),
            ));
        }
        // TODO Validate the version info
        Success(VersionInput { value: string })
    }
}

impl<'a, 'r> FromRequest<'a, 'r> for Reader {
    type Error = HandlerError;

    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, HandlerError> {
        auth::authorized_reader(request).into_outcome(VALIDATION_FAILED)
    }
}

// REST Functions

/// Set a version for a broadcaster / bchannel
#[post("/v1/broadcasts/<_broadcaster_id>/<bchannel_id>", data = "<version>")]
fn broadcast(
    conn: db::Conn,
    broadcaster: HandlerResult<Broadcaster>,
    _broadcaster_id: String,
    bchannel_id: String,
    version: HandlerResult<VersionInput>,
) -> HandlerResult<Json> {
    broadcaster?.new_broadcast(&conn, bchannel_id, version?.value)?;
    Ok(Json(json!({
        "code": 200
    })))
}

/// Dump the current version table
#[get("/v1/broadcasts")]
fn get_broadcasts(conn: db::Conn, reader: HandlerResult<Reader>) -> HandlerResult<Json> {
    let broadcasts = reader?.read_broadcasts(&conn)?;
    Ok(Json(json!({
        "code": 200,
        "broadcasts": broadcasts
    })))
}

#[get("/__version__")]
fn version() -> content::Json<&'static str> {
    content::Json(include_str!("../version.json"))
}

#[get("/__heartbeat__")]
fn heartbeat(conn: db::Conn) -> status::Custom<Json> {
    let (status, db_msg) = match broadcastsv1::table
        .select(sql::<Integer>("1"))
        .get_result::<i32>(&*conn)
    {
        Ok(_) | Err(DieselError::NotFound) => (Status::Ok, "ok".to_string()),
        // XXX: sanitize db_msg
        Err(e) => (Status::ServiceUnavailable, e.description().to_string()),
    };

    status::Custom(
        status,
        Json(json!({
            "status": if status == Status::Ok { "ok" } else { "error" },
            "code": status.code,
            "database": db_msg
        })),
    )
}

#[get("/__lheartbeat__")]
fn lheartbeat() {}

#[error(404)]
fn not_found() -> HandlerResult<()> {
    Err(HandlerErrorKind::NotFound)?
}

pub fn rocket() -> Result<Rocket> {
    let rocket = rocket::ignite();
    let pool = db::pool_from_config(rocket.config())?;
    let authenticator = auth::BearerTokenAuthenticator::from_config(rocket.config())?;
    Ok(rocket
        .manage(pool)
        .manage(authenticator)
        .mount(
            "/",
            routes![broadcast, get_broadcasts, version, heartbeat, lheartbeat],
        )
        .catch(errors![not_found]))
}

#[cfg(test)]
mod test {
    use std::env;

    use diesel::Connection;
    use rocket::local::Client;
    use rocket::http::{Header, Status};
    use rocket::response::Response;
    use serde_json::{self, Value};

    use db::MysqlPool;
    use super::rocket;

    /// Test auth headers
    enum Auth {
        Foo,
        Baz,
        Reader,
    }

    impl Into<Header<'static>> for Auth {
        fn into(self) -> Header<'static> {
            let token = match self {
                Auth::Foo => "feedfacedeadbeef",
                Auth::Baz => "deadbeeffacefeed",
                Auth::Reader => "00000000deadbeef",
            };
            Header::new("Authorization".to_string(), format!("Bearer {}", token))
        }
    }

    /// Return a Rocket Client for testing
    ///
    /// The managed db pool is set to a maxiumum of one connection w/
    /// a transaction began that is never committed
    fn rocket_client() -> Client {
        // hacky/easiest way to set into rocket's config
        env::set_var("ROCKET_DATABASE_POOL_MAX_SIZE", "1");
        env::set_var(
            "ROCKET_BROADCASTER_AUTH",
            r#"{foo=["feedfacedeadbeef"], baz=["deadbeeffacefeed"]}"#,
        );
        env::set_var("ROCKET_READER_AUTH", r#"{reader=["00000000deadbeef"]}"#);

        let rocket = rocket().expect("rocket failed");
        {
            let pool = rocket.state::<MysqlPool>().unwrap();
            let conn = &*pool.get().expect("Couldn't connect to database");
            conn.begin_test_transaction().unwrap();
        }
        Client::new(rocket).expect("rocket launch failed")
    }

    fn json_body(response: &mut Response) -> Value {
        assert!(response.content_type().map_or(false, |ct| ct.is_json()));
        serde_json::from_str(&response.body_string().unwrap()).unwrap()
    }

    #[test]
    fn test_post() {
        let client = rocket_client();
        let mut response = client
            .post("/v1/broadcasts/foo/bar")
            .header(Auth::Foo)
            .body("v1")
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(json_body(&mut response), json!({"code": 200}));
    }

    #[test]
    fn test_post_no_body() {
        let client = rocket_client();
        let mut response = client
            .post("/v1/broadcasts/foo/bar")
            .header(Auth::Foo)
            .dispatch();
        assert_eq!(response.status(), Status::BadRequest);
        let result = json_body(&mut response);
        assert_eq!(result["code"], Status::BadRequest.code);
        assert!(result["error"].as_str().unwrap().contains("Version"));
    }

    #[test]
    fn test_post_no_id() {
        let client = rocket_client();
        let mut response = client
            .post("/v1/broadcasts/foo")
            .header(Auth::Foo)
            .body("v1")
            .dispatch();
        assert_eq!(response.status(), Status::NotFound);
        assert_eq!(
            json_body(&mut response),
            json!({"code": 404, "error": "Not Found"})
        );
    }

    #[test]
    fn test_post_no_auth() {
        let client = rocket_client();
        let mut response = client.post("/v1/broadcasts/foo/bar").body("v1").dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let result = json_body(&mut response);
        assert_eq!(result["code"], 401);
    }

    #[test]
    fn test_post_bad_auth() {
        let client = rocket_client();
        let mut response = client
            .post("/v1/broadcasts/foo/bar")
            .header(Auth::Baz)
            .body("v1")
            .dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let result = json_body(&mut response);
        assert_eq!(result["code"], 401);
    }

    #[test]
    fn test_get_no_auth() {
        let client = rocket_client();
        let mut response = client.get("/v1/broadcasts").dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let result = json_body(&mut response);
        assert_eq!(result["code"], 401);
    }

    #[test]
    fn test_get_bad_auth() {
        let client = rocket_client();
        let mut response = client.get("/v1/broadcasts").header(Auth::Foo).dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let result = json_body(&mut response);
        assert_eq!(result["code"], 401);
    }

    #[test]
    fn test_post_get() {
        let client = rocket_client();
        let _ = client
            .post("/v1/broadcasts/foo/bar")
            .header(Auth::Foo)
            .body("v1")
            .dispatch();
        let _ = client
            .post("/v1/broadcasts/baz/quux")
            .header(Auth::Baz)
            .body("v0")
            .dispatch();
        let mut response = client.get("/v1/broadcasts").header(Auth::Reader).dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            json_body(&mut response),
            json!({"code": 200, "broadcasts": {"baz/quux": "v0", "foo/bar": "v1"}})
        );
    }

    #[test]
    fn test_version() {
        let client = rocket_client();
        let mut response = client.get("/__version__").dispatch();
        assert_eq!(response.status(), Status::Ok);
        let result = json_body(&mut response);
        assert_eq!(result["version"], "devel");
    }

    #[test]
    fn test_heartbeat() {
        let client = rocket_client();
        let mut response = client.get("/__heartbeat__").dispatch();
        assert_eq!(response.status(), Status::Ok);
        let result = json_body(&mut response);
        assert_eq!(result["code"], 200);
        assert_eq!(result["database"], "ok");
    }

    #[test]
    fn test_lheartbeat() {
        let client = rocket_client();
        let mut response = client.get("/__lheartbeat__").dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert!(response.body().is_none());
    }
}
