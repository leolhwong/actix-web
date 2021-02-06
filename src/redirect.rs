//! See [`Redirect`] for service documentation.

use std::future::ready;
​
use actix_service::fn_service;
use actix_web::{
    dev::{AppService, HttpServiceFactory, ResourceDef, ServiceRequest},
    http::{header, StatusCode},
    HttpResponse,
};
​
/// Determines how redirects are routed.
#[derive(Debug, Clone)]
enum RedirectType {
    /// An absolute path or full URL used as-is when redirecting.
    Absolute(String),
​
    /// A path relative to matched path.
    Relative(String),
}
​
/// An HTTP service for redirecting one path to another path or URL.
///
/// Redirects are either [relative](Redirect::to_relative) or [absolute](Redirect::to_absolute).
///
/// By default, the "308 Temporary Redirect" status is used when responding.
/// See [this MDN article](mdn-redirects) on why 308 is preferred over 301.
///
/// # Examples
/// ```
/// App::new()
///     // redirect "/duck" to DuckDuckGo
///     .service(Redirect::from("/duck").to_absolute("https://duckduckgo.com/"))
///     .service(
///         // redirect "/api/old" to "/api/new"
///         web::scope("/api").service(Redirect::from("/old").to_relative("/new"))
///     )
/// ```
///
/// [mdn-redirects]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Redirections#temporary_redirections
#[derive(Debug, Clone)]
pub struct Redirect {
    from: String,
    to: RedirectType,
    status_code: StatusCode,
}
​
impl Redirect {
    /// Create a new `Redirect` service, first providing the path that should be redirected.
    ///
    /// The default "to" location is the root path (`/`). It is expected that you should call either
    /// [`to_relative`](Redirect::to_relative) or [`to_absolute`](Redirect::to_absolute) afterwards.
    pub fn from(from: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: RedirectType::Absolute("/".to_owned()),
            status_code: StatusCode::PERMANENT_REDIRECT,
        }
    }
​
    /// Redirect to an absolute address or path.
    ///
    /// Whatever argument is provided shall be used as-is when setting the redirect location.
    #[allow(dead_code, clippy::wrong_self_convention)]
    pub fn to_absolute(mut self, to: impl Into<String>) -> Self {
        self.to = RedirectType::Absolute(to.into());
        self
    }
​
    /// Redirect to a relative path.
    ///
    /// The provided argument will replace
    #[allow(clippy::wrong_self_convention)]
    pub fn to_relative(mut self, to: impl Into<String>) -> Self {
        self.to = RedirectType::Relative(to.into());
        self
    }
​
    /// Use the "307 Temporary Redirect" status when responding.
    ///
    /// See [this MDN article](mdn-redirects) on why 307 is preferred over 302.
    ///
    /// [mdn-redirects]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Redirections#temporary_redirections
    #[allow(dead_code)]
    pub fn temporary(self) -> Self {
        self.using_status_code(StatusCode::TEMPORARY_REDIRECT)
    }
​
    /// Allows the use of custom status codes for less common redirect types.
    ///
    /// In most cases, the default status ("308 Permanent Redirect") or using the `temporary`
    /// method, which uses the "307 Temporary Redirect" status have more consistent behavior than
    /// 301 and 302 codes, respectively.
    ///
    /// ```
    /// # use ancile::helpers::Redirect;
    /// // redirects would use "301 Moved Permanently" status code
    /// Redirect::from("/old")
    ///     .to_relative("/new")
    ///     .using_status_code(StatusCode::MOVED_PERMANENTLY);
    ///
    /// // redirects would use "302 Found" status code
    /// Redirect::from("/old")
    ///     .to_relative("/new")
    ///     .using_status_code(StatusCode::FOUND);
    /// ```
    #[allow(dead_code)]
    pub fn using_status_code(mut self, status: StatusCode) -> Self {
        self.status_code = status;
        self
    }
}
​
impl HttpServiceFactory for Redirect {
    fn register(self, config: &mut AppService) {
        let Self {
            from,
            to,
            status_code,
        } = self;
​
        let rdef = ResourceDef::new(from.clone());
        let redirect_factory = fn_service(move |req: ServiceRequest| {
            let uri = req.uri().to_string();
​
            let redirect_to = match &to {
                RedirectType::Absolute(to) => to.clone(),
                RedirectType::Relative(to) => {
                    // if service matched then suffix can always be stripped
                    let uri = uri.strip_suffix(&from).unwrap();
​
                    let mut redirect_to = uri.to_owned();
                    redirect_to.push_str(&to.clone());
                    redirect_to
                }
            };
​
            ready(Ok(req.into_response(
                HttpResponse::build(status_code)
                    .header(header::LOCATION, redirect_to)
                    .finish(),
            )))
        });
​
        config.register_service(rdef, None, redirect_factory, None)
    }
}
​
#[cfg(test)]
mod tests {
    use super::*;
​
    use actix_web::{
        dev::Service,
        http::StatusCode,
        test::{self, TestRequest},
        web, App,
    };
​
    #[actix_rt::test]
    async fn absolute_redirects() {
        let redirector = Redirect::from("/one").to_absolute("/two");
​
        let mut svc = test::init_service(
            App::new()
                .service(web::scope("/scoped").service(redirector.clone()))
                .service(redirector),
        )
        .await;
​
        let req = TestRequest::default().uri("/one").to_request();
        let res = svc.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(308).unwrap());
        let hdr = res.headers().get(&header::LOCATION).unwrap();
        assert_eq!(hdr.to_str().unwrap(), "/two");
​
        let req = TestRequest::default().uri("/scoped/one").to_request();
        let res = svc.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(308).unwrap());
        let hdr = res.headers().get(&header::LOCATION).unwrap();
        assert_eq!(hdr.to_str().unwrap(), "/two");
    }
​
    #[actix_rt::test]
    async fn relative_redirects() {
        let redirector = Redirect::from("/one").to_relative("/two");
​
        let mut svc = test::init_service(
            App::new()
                .service(web::scope("/scoped").service(redirector.clone()))
                .service(redirector),
        )
        .await;
​
        let req = TestRequest::default().uri("/one").to_request();
        let res = svc.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(308).unwrap());
        let hdr = res.headers().get(&header::LOCATION).unwrap();
        assert_eq!(hdr.to_str().unwrap(), "/two");
​
        let req = TestRequest::default().uri("/scoped/one").to_request();
        let res = svc.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(308).unwrap());
        let hdr = res.headers().get(&header::LOCATION).unwrap();
        assert_eq!(hdr.to_str().unwrap(), "/scoped/two");
    }
​
    #[actix_rt::test]
    async fn temporary_redirects() {
        let external_service = Redirect::from("/external")
            .to_absolute("https://duck.com")
            .temporary();
​
        let mut svc = test::init_service(App::new().service(external_service)).await;
​
        let req = TestRequest::default().uri("/external").to_request();
        let res = svc.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(307).unwrap());
        let hdr = res.headers().get(&header::LOCATION).unwrap();
        assert_eq!(hdr.to_str().unwrap(), "https://duck.com");
    }
}
