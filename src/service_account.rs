//! This module provides a token source (`GetToken`) that obtains tokens for service accounts.
//! Service accounts are usually used by software (i.e., non-human actors) to get access to
//! resources. Currently, this module only works with RS256 JWTs, which makes it at least suitable for
//! authentication with Google services.
//!
//! Resources:
//! - [Using OAuth 2.0 for Server to Server
//! Applications](https://developers.google.com/identity/protocols/OAuth2ServiceAccount)
//! - [JSON Web Tokens](https://jwt.io/)
//!
//! Copyright (c) 2016 Google Inc (lewinb@google.com).
//!

use std::default::Default;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::authenticator::{DefaultHyperClient, HyperClientBuilder};
use crate::storage::{hash_scopes, MemoryStorage, TokenStorage};
use crate::types::{ApplicationSecret, GetToken, JsonErrorOr, RequestError, Token};

use futures::prelude::*;
use hyper::header;
use url::form_urlencoded;

use rustls::{
    self,
    internal::pemfile,
    sign::{self, SigningKey},
    PrivateKey,
};
use std::io;

use base64;
use chrono;
use hyper;
use serde_json;

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const GOOGLE_RS256_HEAD: &str = r#"{"alg":"RS256","typ":"JWT"}"#;

/// Encodes s as Base64
fn append_base64<T: AsRef<[u8]> + ?Sized>(s: &T, out: &mut String) {
    base64::encode_config_buf(s, base64::URL_SAFE, out)
}

/// Decode a PKCS8 formatted RSA key.
fn decode_rsa_key(pem_pkcs8: &str) -> Result<PrivateKey, io::Error> {
    let private = pem_pkcs8.to_string().replace("\\n", "\n").into_bytes();
    let mut private_reader: &[u8] = private.as_ref();
    let private_keys = pemfile::pkcs8_private_keys(&mut private_reader);

    if let Ok(pk) = private_keys {
        if !pk.is_empty() {
            Ok(pk[0].clone())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not enough private keys in PEM",
            ))
        }
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Error reading key from PEM",
        ))
    }
}

/// JSON schema of secret service account key. You can obtain the key from
/// the Cloud Console at https://console.cloud.google.com/.
///
/// You can use `helpers::service_account_key_from_file()` as a quick way to read a JSON client
/// secret into a ServiceAccountKey.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServiceAccountKey {
    #[serde(rename = "type")]
    pub key_type: Option<String>,
    pub project_id: Option<String>,
    pub private_key_id: Option<String>,
    pub private_key: String,
    pub client_email: String,
    pub client_id: Option<String>,
    pub auth_uri: Option<String>,
    pub token_uri: String,
    pub auth_provider_x509_cert_url: Option<String>,
    pub client_x509_cert_url: Option<String>,
}

/// Permissions requested for a JWT.
/// See https://developers.google.com/identity/protocols/OAuth2ServiceAccount#authorizingrequests.
#[derive(Serialize, Debug)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    exp: i64,
    iat: i64,
    subject: Option<&'a str>,
    scope: String,
}

impl<'a> Claims<'a> {
    fn new<T>(key: &'a ServiceAccountKey, scopes: &[T], subject: Option<&'a str>) -> Self
    where
        T: AsRef<str>,
    {
        let iat = chrono::Utc::now().timestamp();
        let expiry = iat + 3600 - 5; // Max validity is 1h.

        let scope = crate::helper::join(scopes, " ");
        Claims {
            iss: &key.client_email,
            aud: &key.token_uri,
            exp: expiry,
            iat,
            subject,
            scope,
        }
    }
}

/// A JSON Web Token ready for signing.
struct JWTSigner {
    signer: Box<dyn rustls::sign::Signer>,
}

impl JWTSigner {
    fn new(private_key: &str) -> Result<Self, io::Error> {
        let key = decode_rsa_key(private_key)?;
        let signing_key = sign::RSASigningKey::new(&key)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Couldn't initialize signer"))?;
        let signer = signing_key
            .choose_scheme(&[rustls::SignatureScheme::RSA_PKCS1_SHA256])
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "Couldn't choose signing scheme")
            })?;
        Ok(JWTSigner { signer })
    }

    fn sign_claims(&self, claims: &Claims) -> Result<String, rustls::TLSError> {
        let mut jwt_head = Self::encode_claims(claims);
        let signature = self.signer.sign(jwt_head.as_bytes())?;
        jwt_head.push_str(".");
        append_base64(&signature, &mut jwt_head);
        Ok(jwt_head)
    }

    /// Encodes the first two parts (header and claims) to base64 and assembles them into a form
    /// ready to be signed.
    fn encode_claims(claims: &Claims) -> String {
        let mut head = String::new();
        append_base64(GOOGLE_RS256_HEAD, &mut head);
        head.push_str(".");
        append_base64(&serde_json::to_string(&claims).unwrap(), &mut head);
        head
    }
}

/// A token source (`GetToken`) yielding OAuth tokens for services that use ServiceAccount authorization.
/// This token source caches token and automatically renews expired ones, meaning you do not need
/// (and you also should not) use this with `Authenticator`. Just use it directly.
#[derive(Clone)]
pub struct ServiceAccountAccess<C> {
    client: C,
    key: ServiceAccountKey,
    subject: Option<String>,
}

impl ServiceAccountAccess<DefaultHyperClient> {
    /// Create a new ServiceAccountAccess with the provided key.
    pub fn new(key: ServiceAccountKey) -> Self {
        ServiceAccountAccess {
            client: DefaultHyperClient,
            key,
            subject: None,
        }
    }
}

impl<C> ServiceAccountAccess<C>
where
    C: HyperClientBuilder,
{
    /// Use the provided hyper client.
    pub fn hyper_client<NewC: HyperClientBuilder>(
        self,
        hyper_client: NewC,
    ) -> ServiceAccountAccess<NewC> {
        ServiceAccountAccess {
            client: hyper_client,
            key: self.key,
            subject: self.subject,
        }
    }

    /// Use the provided subject.
    pub fn subject(self, subject: String) -> Self {
        ServiceAccountAccess {
            subject: Some(subject),
            ..self
        }
    }

    /// Build the configured ServiceAccountAccess.
    pub fn build(self) -> Result<impl GetToken, io::Error> {
        ServiceAccountAccessImpl::new(self.client.build_hyper_client(), self.key, self.subject)
    }
}

struct ServiceAccountAccessImpl<C> {
    client: hyper::Client<C, hyper::Body>,
    key: ServiceAccountKey,
    cache: Arc<Mutex<MemoryStorage>>,
    subject: Option<String>,
    signer: JWTSigner,
}

impl<C> ServiceAccountAccessImpl<C>
where
    C: hyper::client::connect::Connect,
{
    fn new(
        client: hyper::Client<C>,
        key: ServiceAccountKey,
        subject: Option<String>,
    ) -> Result<Self, io::Error> {
        let signer = JWTSigner::new(&key.private_key)?;
        Ok(ServiceAccountAccessImpl {
            client,
            key,
            cache: Arc::new(Mutex::new(MemoryStorage::default())),
            subject,
            signer,
        })
    }
}

/// This is the schema of the server's response.
#[derive(Deserialize, Debug)]
struct TokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
}

impl<C> ServiceAccountAccessImpl<C>
where
    C: hyper::client::connect::Connect + 'static,
{
    /// Send a request for a new Bearer token to the OAuth provider.
    async fn request_token<T>(
        client: &hyper::client::Client<C>,
        signer: &JWTSigner,
        subject: Option<&str>,
        key: &ServiceAccountKey,
        scopes: &[T],
    ) -> Result<Token, RequestError>
    where
        T: AsRef<str>,
    {
        let claims = Claims::new(key, scopes, subject);
        let signed = signer.sign_claims(&claims).map_err(|_| {
            RequestError::LowLevelError(io::Error::new(
                io::ErrorKind::Other,
                "unable to sign claims",
            ))
        })?;
        let rqbody = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&[("grant_type", GRANT_TYPE), ("assertion", signed.as_str())])
            .finish();
        let request = hyper::Request::post(&key.token_uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(hyper::Body::from(rqbody))
            .unwrap();
        let response = client
            .request(request)
            .await
            .map_err(RequestError::ClientError)?;
        let body = response
            .into_body()
            .try_concat()
            .await
            .map_err(RequestError::ClientError)?;
        match serde_json::from_slice::<JsonErrorOr<TokenResponse>>(&body)? {
            JsonErrorOr::Err(err) => Err(err.into()),
            JsonErrorOr::Data(TokenResponse {
                access_token: Some(access_token),
                token_type: Some(token_type),
                expires_in: Some(expires_in),
                ..
            }) => {
                let expires_ts = chrono::Utc::now().timestamp() + expires_in;
                Ok(Token {
                    access_token,
                    token_type,
                    refresh_token: None,
                    expires_in: Some(expires_in),
                    expires_in_timestamp: Some(expires_ts),
                })
            }
            JsonErrorOr::Data(token) => Err(RequestError::BadServerResponse(format!(
                "Token response lacks fields: {:?}",
                token
            ))),
        }
    }

    async fn get_token<T>(&self, scopes: &[T]) -> Result<Token, RequestError>
    where
        T: AsRef<str>,
    {
        let hash = hash_scopes(scopes);
        let cache = &self.cache;
        match cache.lock().unwrap().get(hash, scopes) {
            Ok(Some(token)) if !token.expired() => return Ok(token),
            _ => {}
        }
        let token = Self::request_token(
            &self.client,
            &self.signer,
            self.subject.as_ref().map(|x| x.as_str()),
            &self.key,
            scopes,
        )
        .await?;
        let _ = cache.lock().unwrap().set(hash, scopes, Some(token.clone()));
        Ok(token)
    }
}

impl<C> GetToken for ServiceAccountAccessImpl<C>
where
    C: hyper::client::connect::Connect + 'static,
{
    fn token<'a, T>(
        &'a self,
        scopes: &'a [T],
    ) -> Pin<Box<dyn Future<Output = Result<Token, RequestError>> + Send + 'a>>
    where
        T: AsRef<str> + Sync,
    {
        Box::pin(self.get_token(scopes))
    }

    /// Returns an empty ApplicationSecret as tokens for service accounts don't need to be
    /// refreshed (they are simply reissued).
    fn application_secret(&self) -> &ApplicationSecret {
        static APP_SECRET: ApplicationSecret = ApplicationSecret::empty();
        &APP_SECRET
    }

    fn api_key(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::service_account_key_from_file;
    use crate::types::GetToken;

    use hyper;
    use hyper_rustls::HttpsConnector;
    use mockito::{self, mock};
    use tokio;

    #[test]
    fn test_mocked_http() {
        env_logger::try_init().unwrap();
        let server_url = &mockito::server_url();
        let client_secret = r#"{
  "type": "service_account",
  "project_id": "yup-test-243420",
  "private_key_id": "26de294916614a5ebdf7a065307ed3ea9941902b",
  "private_key": "-----BEGIN PRIVATE KEY-----\nMIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDemmylrvp1KcOn\n9yTAVVKPpnpYznvBvcAU8Qjwr2fSKylpn7FQI54wCk5VJVom0jHpAmhxDmNiP8yv\nHaqsef+87Oc0n1yZ71/IbeRcHZc2OBB33/LCFqf272kThyJo3qspEqhuAw0e8neg\nLQb4jpm9PsqR8IjOoAtXQSu3j0zkXemMYFy93PWHjVpPEUX16NGfsWH7oxspBHOk\n9JPGJL8VJdbiAoDSDgF0y9RjJY5I52UeHNhMsAkTYs6mIG4kKXt2+T9tAyHw8aho\nwmuytQAfydTflTfTG8abRtliF3nil2taAc5VB07dP1b4dVYy/9r6M8Z0z4XM7aP+\nNdn2TKm3AgMBAAECggEAWi54nqTlXcr2M5l535uRb5Xz0f+Q/pv3ceR2iT+ekXQf\n+mUSShOr9e1u76rKu5iDVNE/a7H3DGopa7ZamzZvp2PYhSacttZV2RbAIZtxU6th\n7JajPAM+t9klGh6wj4jKEcE30B3XVnbHhPJI9TCcUyFZoscuPXt0LLy/z8Uz0v4B\nd5JARwyxDMb53VXwukQ8nNY2jP7WtUig6zwE5lWBPFMbi8GwGkeGZOruAK5sPPwY\nGBAlfofKANI7xKx9UXhRwisB4+/XI1L0Q6xJySv9P+IAhDUI6z6kxR+WkyT/YpG3\nX9gSZJc7qEaxTIuDjtep9GTaoEqiGntjaFBRKoe+VQKBgQDzM1+Ii+REQqrGlUJo\nx7KiVNAIY/zggu866VyziU6h5wjpsoW+2Npv6Dv7nWvsvFodrwe50Y3IzKtquIal\nVd8aa50E72JNImtK/o5Nx6xK0VySjHX6cyKENxHRDnBmNfbALRM+vbD9zMD0lz2q\nmns/RwRGq3/98EqxP+nHgHSr9QKBgQDqUYsFAAfvfT4I75Glc9svRv8IsaemOm07\nW1LCwPnj1MWOhsTxpNF23YmCBupZGZPSBFQobgmHVjQ3AIo6I2ioV6A+G2Xq/JCF\nmzfbvZfqtbbd+nVgF9Jr1Ic5T4thQhAvDHGUN77BpjEqZCQLAnUWJx9x7e2xvuBl\n1A6XDwH/ewKBgQDv4hVyNyIR3nxaYjFd7tQZYHTOQenVffEAd9wzTtVbxuo4sRlR\nNM7JIRXBSvaATQzKSLHjLHqgvJi8LITLIlds1QbNLl4U3UVddJbiy3f7WGTqPFfG\nkLhUF4mgXpCpkMLxrcRU14Bz5vnQiDmQRM4ajS7/kfwue00BZpxuZxst3QKBgQCI\nRI3FhaQXyc0m4zPfdYYVc4NjqfVmfXoC1/REYHey4I1XetbT9Nb/+ow6ew0UbgSC\nUZQjwwJ1m1NYXU8FyovVwsfk9ogJ5YGiwYb1msfbbnv/keVq0c/Ed9+AG9th30qM\nIf93hAfClITpMz2mzXIMRQpLdmQSR4A2l+E4RjkSOwKBgQCB78AyIdIHSkDAnCxz\nupJjhxEhtQ88uoADxRoEga7H/2OFmmPsqfytU4+TWIdal4K+nBCBWRvAX1cU47vH\nJOlSOZI0gRKe0O4bRBQc8GXJn/ubhYSxI02IgkdGrIKpOb5GG10m85ZvqsXw3bKn\nRVHMD0ObF5iORjZUqD0yRitAdg==\n-----END PRIVATE KEY-----\n",
  "client_email": "yup-test-sa-1@yup-test-243420.iam.gserviceaccount.com",
  "client_id": "102851967901799660408",
  "auth_uri": "https://accounts.google.com/o/oauth2/auth",
  "token_uri": "",
  "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
  "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/yup-test-sa-1%40yup-test-243420.iam.gserviceaccount.com"
}"#;
        let mut key: ServiceAccountKey = serde_json::from_str(client_secret).unwrap();
        key.token_uri = format!("{}/token", server_url);

        let json_response = r#"{
  "access_token": "ya29.c.ElouBywiys0LyNaZoLPJcp1Fdi2KjFMxzvYKLXkTdvM-rDfqKlvEq6PiMhGoGHx97t5FAvz3eb_ahdwlBjSStxHtDVQB4ZPRJQ_EOi-iS7PnayahU2S9Jp8S6rk",
  "expires_in": 3600,
  "token_type": "Bearer"
}"#;
        let bad_json_response = r#"{
  "access_token": "ya29.c.ElouBywiys0LyNaZoLPJcp1Fdi2KjFMxzvYKLXkTdvM-rDfqKlvEq6PiMhGoGHx97t5FAvz3eb_ahdwlBjSStxHtDVQB4ZPRJQ_EOi-iS7PnayahU2S9Jp8S6rk",
  "token_type": "Bearer"
}"#;

        let https = HttpsConnector::new();
        let client = hyper::Client::builder()
            .keep_alive(false)
            .build::<_, hyper::Body>(https);
        let rt = tokio::runtime::Builder::new()
            .core_threads(1)
            .panic_handler(|e| std::panic::resume_unwind(e))
            .build()
            .unwrap();

        // Successful path.
        {
            let _m = mock("POST", "/token")
                .with_status(200)
                .with_header("content-type", "text/json")
                .with_body(json_response)
                .expect(1)
                .create();
            let acc = ServiceAccountAccessImpl::new(client.clone(), key.clone(), None).unwrap();
            let fut = async {
                let tok = acc
                    .token(&["https://www.googleapis.com/auth/pubsub"])
                    .await?;
                assert!(tok.access_token.contains("ya29.c.ElouBywiys0Ly"));
                assert_eq!(Some(3600), tok.expires_in);
                Ok(()) as Result<(), RequestError>
            };
            rt.block_on(fut).expect("block_on");

            assert!(acc
                .cache
                .lock()
                .unwrap()
                .get(
                    3502164897243251857,
                    &["https://www.googleapis.com/auth/pubsub"],
                )
                .unwrap()
                .is_some());
            // Test that token is in cache (otherwise mock will tell us)
            let fut = async {
                let tok = acc
                    .token(&["https://www.googleapis.com/auth/pubsub"])
                    .await?;
                assert!(tok.access_token.contains("ya29.c.ElouBywiys0Ly"));
                assert_eq!(Some(3600), tok.expires_in);
                Ok(()) as Result<(), RequestError>
            };
            rt.block_on(fut).expect("block_on 2");

            _m.assert();
        }
        // Malformed response.
        {
            let _m = mock("POST", "/token")
                .with_status(200)
                .with_header("content-type", "text/json")
                .with_body(bad_json_response)
                .create();
            let acc = ServiceAccountAccess::new(key.clone())
                .hyper_client(client.clone())
                .build()
                .unwrap();
            let fut = async {
                let result = acc.token(&["https://www.googleapis.com/auth/pubsub"]).await;
                assert!(result.is_err());
                Ok(()) as Result<(), ()>
            };
            rt.block_on(fut).expect("block_on");
            _m.assert();
        }
        rt.shutdown_on_idle();
    }

    // Valid but deactivated key.
    const TEST_PRIVATE_KEY_PATH: &'static str = "examples/Sanguine-69411a0c0eea.json";

    // Uncomment this test to verify that we can successfully obtain tokens.
    //#[test]
    #[allow(dead_code)]
    fn test_service_account_e2e() {
        let key = service_account_key_from_file(&TEST_PRIVATE_KEY_PATH.to_string()).unwrap();
        let https = HttpsConnector::new();
        let client = hyper::Client::builder().build(https);
        let acc = ServiceAccountAccess::new(key)
            .hyper_client(client)
            .build()
            .unwrap();
        let rt = tokio::runtime::Builder::new()
            .core_threads(1)
            .panic_handler(|e| std::panic::resume_unwind(e))
            .build()
            .unwrap();
        rt.block_on(async {
            println!(
                "{:?}",
                acc.token(&["https://www.googleapis.com/auth/pubsub"]).await
            );
        });
    }

    #[test]
    fn test_jwt_initialize_claims() {
        let key = service_account_key_from_file(TEST_PRIVATE_KEY_PATH).unwrap();
        let scopes = vec!["scope1", "scope2", "scope3"];
        let claims = Claims::new(&key, &scopes, None);

        assert_eq!(
            claims.iss,
            "oauth2-public-test@sanguine-rhythm-105020.iam.gserviceaccount.com".to_string()
        );
        assert_eq!(claims.scope, "scope1 scope2 scope3".to_string());
        assert_eq!(
            claims.aud,
            "https://accounts.google.com/o/oauth2/token".to_string()
        );
        assert!(claims.exp > 1000000000);
        assert!(claims.iat < claims.exp);
        assert_eq!(claims.exp - claims.iat, 3595);
    }

    #[test]
    fn test_jwt_sign() {
        let key = service_account_key_from_file(TEST_PRIVATE_KEY_PATH).unwrap();
        let scopes = vec!["scope1", "scope2", "scope3"];
        let signer = JWTSigner::new(&key.private_key).unwrap();
        let claims = Claims::new(&key, &scopes, None);
        let signature = signer.sign_claims(&claims);

        assert!(signature.is_ok());

        let signature = signature.unwrap();
        assert_eq!(
            signature.split(".").nth(0).unwrap(),
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9"
        );
    }
}
