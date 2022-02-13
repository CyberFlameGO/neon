use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use std::net::{SocketAddr, ToSocketAddrs};
use std::collections::HashMap;

use crate::state::ProxyWaiters;

#[derive(Debug, PartialEq, Eq)]
pub struct ClientCredentials {
    pub user: String,
    pub dbname: String,
}

impl TryFrom<HashMap<String, String>> for ClientCredentials {
    type Error = anyhow::Error;

    fn try_from(mut value: HashMap<String, String>) -> Result<Self, Self::Error> {
        let mut get_param = |key| {
            value
                .remove(key)
                .with_context(|| format!("{} is missing in startup packet", key))
        };

        let user = get_param("user")?;
        let db = get_param("database")?;

        Ok(Self { user, dbname: db })
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct DatabaseInfo {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum ProxyAuthResponse {
    Ready { conn_info: DatabaseInfo },
    Error { error: String },
    NotReady { ready: bool }, // TODO: get rid of `ready`
}

impl DatabaseInfo {
    pub fn socket_addr(&self) -> anyhow::Result<SocketAddr> {
        let host_port = format!("{}:{}", self.host, self.port);
        host_port
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {} to SocketAddr", host_port))?
            .next()
            .context("cannot resolve at least one SocketAddr")
    }
}

impl From<DatabaseInfo> for tokio_postgres::Config {
    fn from(db_info: DatabaseInfo) -> Self {
        let mut config = tokio_postgres::Config::new();

        config
            .host(&db_info.host)
            .port(db_info.port)
            .dbname(&db_info.dbname)
            .user(&db_info.user);

        if let Some(password) = db_info.password {
            config.password(password);
        }

        config
    }
}

pub struct FullApi<'a> {
    md5_api: Md5Api<'a>,
    link_api: LinkApi<'a>,
}

pub struct Md5Api<'a> {
    auth_endpoint: &'a str,
    waiters: &'a ProxyWaiters,
}

pub struct LinkApi<'a> {
    redirect_uri: &'a str,
    waiters: &'a ProxyWaiters,
}


impl<'a> Md5Api<'a> {
    pub fn new(auth_endpoint: &'a str, waiters: &'a ProxyWaiters) -> Self {
        Self {
            auth_endpoint,
            waiters,
        }
    }
}

impl Md5Api<'_> {
    pub async fn authenticate_proxy_request(
        &self,
        user: &str,
        database: &str,
        md5_response: &[u8],
        salt: &[u8; 4],
    ) -> anyhow::Result<DatabaseInfo> {
        let mut url = reqwest::Url::parse(self.auth_endpoint)?;
        let psql_session_id = hex::encode(rand::random::<[u8; 8]>());
        url.query_pairs_mut()
            .append_pair("login", user)
            .append_pair("database", database)
            .append_pair("md5response", std::str::from_utf8(md5_response)?)
            .append_pair("salt", &hex::encode(salt))
            .append_pair("psql_session_id", &psql_session_id);

        let waiter = self.waiters.register(psql_session_id.to_owned());

        println!("cplane request: {}", url);
        let resp = reqwest::get(url).await?;
        if !resp.status().is_success() {
            bail!("Auth failed: {}", resp.status())
        }

        let auth_info = resp.json().await?;
        println!("got auth info: #{:?}", auth_info);

        use ProxyAuthResponse::*;
        match auth_info {
            Ready { conn_info } => Ok(conn_info),
            Error { error } => bail!(error),
            NotReady { .. } => waiter.await.map_err(|e| anyhow!(e)),
        }
    }
}

impl<'a> LinkApi<'a> {
    pub fn new(redirect_uri: &'a str, waiters: &'a ProxyWaiters) -> Self {
        Self {
            redirect_uri,
            waiters,
        }
    }
}

impl LinkApi<'_> {
    pub fn get_hello_message(&self) -> (String, crate::waiters::Waiter<Result<DatabaseInfo, String>>) {
        let session_id = hex::encode(rand::random::<[u8; 8]>());
        let message = format!(
            concat![
                "☀️  Welcome to Zenith!\n",
                "To proceed with database creation, open the following link:\n\n",
                "    {redirect_uri}{session_id}\n\n",
                "It needs to be done once and we will send you '.pgpass' file,\n",
                "which will allow you to access or create ",
                "databases without opening your web browser."
            ],
            redirect_uri = self.redirect_uri,
            session_id = session_id,
        );
        let waiter = self.waiters.register(session_id.clone());
        (message, waiter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_proxy_auth_response() {
        // Ready
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": true,
            "conn_info": DatabaseInfo::default(),
        }))
        .unwrap();
        assert!(matches!(
            auth,
            ProxyAuthResponse::Ready {
                conn_info: DatabaseInfo { .. }
            }
        ));

        // Error
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": false,
            "error": "too bad, so sad",
        }))
        .unwrap();
        assert!(matches!(auth, ProxyAuthResponse::Error { .. }));

        // NotReady
        let auth: ProxyAuthResponse = serde_json::from_value(json!({
            "ready": false,
        }))
        .unwrap();
        assert!(matches!(auth, ProxyAuthResponse::NotReady { .. }));
    }
}
