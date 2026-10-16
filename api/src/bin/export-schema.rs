//! Prints the current GraphQL SDL to stdout. Builds the schema over the mocks
//! (`mockdb` + `mockmail`) — this binary never touches a real database or sends
//! real email, so it can run in CI with no AWS account and no `.env.secret`.
//!
//! `make check` diffs this output against the committed `api/schema.graphql`.

use std::sync::Arc;

use toolbox::app;
use toolbox::graphql;
use toolbox::mockdb;
use toolbox::mockmail;
use toolbox::mockstorage;

fn main() {
    let app = Arc::new(app::new(
        mockdb::Handler::new(),
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        toolbox::mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(app, webauthn);
    print!("{}", schema.sdl());
}
