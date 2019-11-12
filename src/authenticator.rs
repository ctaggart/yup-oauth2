use crate::authenticator_delegate::{AuthenticatorDelegate, DefaultAuthenticatorDelegate, Retry};
use crate::refresh::RefreshFlow;
use crate::storage::{hash_scopes, DiskTokenStorage, MemoryStorage, TokenStorage};
use crate::types::{ApplicationSecret, GetToken, RefreshResult, RequestError, Token};

use futures::prelude::*;

use std::error::Error;
use std::io;
use std::path::Path;
use std::pin::Pin;

/// Authenticator abstracts different `GetToken` implementations behind one type and handles
/// caching received tokens. It's important to use it (instead of the flows directly) because
/// otherwise the user needs to be asked for new authorization every time a token is generated.
///
/// `ServiceAccountAccess` does not need (and does not work) with `Authenticator`, given that it
/// does not require interaction and implements its own caching. Use it directly.
///
/// NOTE: It is recommended to use a client constructed like this in order to prevent functions
/// like `hyper::run()` from hanging: `let client = hyper::Client::builder().keep_alive(false);`.
/// Due to token requests being rare, this should not result in a too bad performance problem.
struct AuthenticatorImpl<
    T: GetToken,
    S: TokenStorage,
    AD: AuthenticatorDelegate,
    C: hyper::client::connect::Connect,
> {
    client: hyper::Client<C>,
    inner: T,
    store: S,
    delegate: AD,
}

/// A trait implemented for any hyper::Client as well as teh DefaultHyperClient.
pub trait HyperClientBuilder {
    type Connector: hyper::client::connect::Connect;

    fn build_hyper_client(self) -> hyper::Client<Self::Connector>;
}

/// The builder value used when the default hyper client should be used.
pub struct DefaultHyperClient;
impl HyperClientBuilder for DefaultHyperClient {
    type Connector = hyper_rustls::HttpsConnector<hyper::client::connect::HttpConnector>;

    fn build_hyper_client(self) -> hyper::Client<Self::Connector> {
        hyper::Client::builder()
            .keep_alive(false)
            .build::<_, hyper::Body>(hyper_rustls::HttpsConnector::new())
    }
}

impl<C> HyperClientBuilder for hyper::Client<C>
where
    C: hyper::client::connect::Connect,
{
    type Connector = C;

    fn build_hyper_client(self) -> hyper::Client<C> {
        self
    }
}

/// An internal trait implemented by flows to be used by an authenticator.
pub trait AuthFlow<C> {
    type TokenGetter: GetToken;

    fn build_token_getter(self, client: hyper::Client<C>) -> Self::TokenGetter;
}

/// An authenticator can be used with `InstalledFlow`'s or `DeviceFlow`'s and
/// will refresh tokens as they expire as well as optionally persist tokens to
/// disk.
pub struct Authenticator<
    T: AuthFlow<C::Connector>,
    S: TokenStorage,
    AD: AuthenticatorDelegate,
    C: HyperClientBuilder,
> {
    client: C,
    token_getter: T,
    store: io::Result<S>,
    delegate: AD,
}

impl<T> Authenticator<T, MemoryStorage, DefaultAuthenticatorDelegate, DefaultHyperClient>
where
    T: AuthFlow<<DefaultHyperClient as HyperClientBuilder>::Connector>,
{
    /// Create a new authenticator with the provided flow. By default a new
    /// hyper::Client will be created the default authenticator delegate will be
    /// used, and tokens will not be persisted to disk.
    /// Accepted flow types are DeviceFlow and InstalledFlow.
    ///
    /// Examples
    /// ```
    /// use std::path::Path;
    /// use yup_oauth2::{ApplicationSecret, Authenticator, DeviceFlow};
    /// let creds = ApplicationSecret::default();
    /// let auth = Authenticator::new(DeviceFlow::new(creds)).build().unwrap();
    /// ```
    pub fn new(
        flow: T,
    ) -> Authenticator<T, MemoryStorage, DefaultAuthenticatorDelegate, DefaultHyperClient> {
        Authenticator {
            client: DefaultHyperClient,
            token_getter: flow,
            store: Ok(MemoryStorage::new()),
            delegate: DefaultAuthenticatorDelegate,
        }
    }
}

impl<T, S, AD, C> Authenticator<T, S, AD, C>
where
    T: AuthFlow<C::Connector>,
    S: TokenStorage,
    AD: AuthenticatorDelegate,
    C: HyperClientBuilder,
{
    /// Use the provided hyper client.
    pub fn hyper_client<NewC>(
        self,
        hyper_client: hyper::Client<NewC>,
    ) -> Authenticator<T, S, AD, hyper::Client<NewC>>
    where
        NewC: hyper::client::connect::Connect,
        T: AuthFlow<NewC>,
    {
        Authenticator {
            client: hyper_client,
            token_getter: self.token_getter,
            store: self.store,
            delegate: self.delegate,
        }
    }

    /// Persist tokens to disk in the provided filename.
    pub fn persist_tokens_to_disk<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Authenticator<T, DiskTokenStorage, AD, C> {
        let disk_storage = DiskTokenStorage::new(path.as_ref().to_str().unwrap());
        Authenticator {
            client: self.client,
            token_getter: self.token_getter,
            store: disk_storage,
            delegate: self.delegate,
        }
    }

    /// Use the provided authenticator delegate.
    pub fn delegate<NewAD: AuthenticatorDelegate>(
        self,
        delegate: NewAD,
    ) -> Authenticator<T, S, NewAD, C> {
        Authenticator {
            client: self.client,
            token_getter: self.token_getter,
            store: self.store,
            delegate: delegate,
        }
    }

    /// Create the authenticator.
    pub fn build(self) -> io::Result<impl GetToken>
    where
        T::TokenGetter: 'static + GetToken,
        S: 'static + Send,
        AD: 'static,
        C::Connector: 'static + Clone + Send,
    {
        let client = self.client.build_hyper_client();
        let store = self.store?;
        let inner = self.token_getter.build_token_getter(client.clone());

        Ok(AuthenticatorImpl {
            client,
            inner,
            store,
            delegate: self.delegate,
        })
    }
}

impl<GT, S, AD, C> AuthenticatorImpl<GT, S, AD, C>
where
    GT: 'static + GetToken,
    S: 'static + TokenStorage,
    AD: 'static + AuthenticatorDelegate,
    C: 'static + hyper::client::connect::Connect + Clone + Send,
{
    async fn get_token<T>(&self, scopes: &[T]) -> Result<Token, RequestError>
    where
        T: AsRef<str> + Sync,
    {
        let scope_key = hash_scopes(scopes);
        let store = &self.store;
        let delegate = &self.delegate;
        let client = &self.client;
        let gettoken = &self.inner;
        let appsecret = gettoken.application_secret();
        loop {
            match store.get(
                scope_key,
                scopes,
            ) {
                Ok(Some(t)) => {
                    if !t.expired() {
                        return Ok(t);
                    }
                    // Implement refresh flow.
                    let refresh_token = t.refresh_token.clone();
                    let rr = RefreshFlow::refresh_token(
                        client,
                        appsecret,
                        refresh_token.unwrap(),
                    )
                    .await?;
                    match rr {
                        RefreshResult::Error(ref e) => {
                            delegate.token_refresh_failed(
                                format!("{}", e.description().to_string()),
                                &Some("the request has likely timed out".to_string()),
                            );
                            return Err(RequestError::Refresh(rr));
                        }
                        RefreshResult::RefreshError(ref s, ref ss) => {
                            delegate.token_refresh_failed(
                                format!("{} {}", s, ss.clone().map(|s| format!("({})", s)).unwrap_or("".to_string())),
                                &Some("the refresh token is likely invalid and your authorization has been revoked".to_string()),
                                );
                            return Err(RequestError::Refresh(rr));
                        }
                        RefreshResult::Success(t) => {
                            let x = store.set(
                                scope_key,
                                scopes,
                                Some(t.clone()),
                            );
                            if let Err(e) = x {
                                match delegate.token_storage_failure(true, &e) {
                                    Retry::Skip => return Ok(t),
                                    Retry::Abort => return Err(RequestError::Cache(Box::new(e))),
                                    Retry::After(d) => tokio::timer::delay_for(d).await,
                                }
                            } else {
                                return Ok(t);
                            }
                        }
                    }
                }
                Ok(None) => {
                    let t = gettoken.token(scopes).await?;
                    if let Err(e) = store.set(
                        scope_key,
                        scopes,
                        Some(t.clone()),
                    ) {
                        match delegate.token_storage_failure(true, &e) {
                            Retry::Skip => return Ok(t),
                            Retry::Abort => return Err(RequestError::Cache(Box::new(e))),
                            Retry::After(d) => tokio::timer::delay_for(d).await,
                        }
                    } else {
                        return Ok(t);
                    }
                }
                Err(err) => match delegate.token_storage_failure(false, &err) {
                    Retry::Abort | Retry::Skip => return Err(RequestError::Cache(Box::new(err))),
                    Retry::After(d) => tokio::timer::delay_for(d).await,
                },
            }
        }
    }
}

impl<
        GT: 'static + GetToken,
        S: 'static + TokenStorage,
        AD: 'static + AuthenticatorDelegate,
        C: 'static + hyper::client::connect::Connect + Clone + Send,
    > GetToken for AuthenticatorImpl<GT, S, AD, C>
{
    /// Returns the API Key of the inner flow.
    fn api_key(&self) -> Option<String> {
        self.inner.api_key()
    }
    /// Returns the application secret of the inner flow.
    fn application_secret(&self) -> &ApplicationSecret {
        self.inner.application_secret()
    }

    fn token<'a, T>(
        &'a self,
        scopes: &'a [T],
    ) -> Pin<Box<dyn Future<Output = Result<Token, RequestError>> + Send + 'a>>
    where
        T: AsRef<str> + Sync,
    {
        Box::pin(self.get_token(scopes))
    }
}
