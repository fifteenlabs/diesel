use mysql_async::prelude::Query;
use mysql_async::{Opts, OptsBuilder};

use super::error_helper::ErrorHelper;

/// The capability to request cancellation of in-progress queries on a
/// connection.
#[derive(Clone)]
// `diesel` warns on `missing_debug_implementations` where `diesel-async`
// did not. These hold a live connection, a cancel handle or a driver
// row; none has a `Debug` worth forwarding.
#[allow(missing_debug_implementations)]
pub struct MysqlCancelToken {
    pub(crate) opts: Opts,
    pub(crate) kill_id: u32,
}

impl MysqlCancelToken {
    /// Attempts to cancel the in-progress query on the connection associated
    /// with this `CancelToken`.
    ///
    /// The server provides no information about whether a cancellation attempt was successful or not. An error will
    /// only be returned if the client was unable to connect to the database.
    ///
    /// Cancellation is inherently racy. There is no guarantee that the
    /// cancellation request will reach the server before the query terminates
    /// normally, or that the connection associated with this token is still
    /// active.
    pub async fn cancel_query(&self) -> crate::result::ConnectionResult<()> {
        let builder = OptsBuilder::from_opts(self.opts.clone());

        let conn = mysql_async::Conn::new(builder).await.map_err(ErrorHelper)?;

        format!("KILL QUERY {};", self.kill_id)
            .ignore(conn)
            .await
            .map_err(ErrorHelper)?;

        Ok(())
    }
}
