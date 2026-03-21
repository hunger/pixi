use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use crate::dev_prefix_pixi::{
    AsyncCall, ConfirmGlobalInstall_Args, GlobalInstall_Args, Info_Args, VarlinkInterface,
};
use crate::server::PixiVarlinkService;

/// A custom `AsyncConnectionHandler` that streams progress replies for
/// `ConfirmGlobalInstall` in real time, while delegating other methods
/// to the normal generated handler logic.
pub struct StreamingHandler {
    inner: Arc<PixiVarlinkService>,
}

impl StreamingHandler {
    pub fn new(inner: Arc<PixiVarlinkService>) -> Self {
        Self { inner }
    }
}

impl varlink::AsyncInterface for StreamingHandler {
    fn get_name(&self) -> &'static str {
        "dev.prefix.pixi"
    }

    fn get_description(&self) -> &'static str {
        include_str!("dev.prefix.pixi.varlink")
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> varlink::Result<T> {
    let Some(params) = params else {
        return Err(varlink::Error(
            varlink::ErrorKind::InvalidParameter("parameters".into()),
            None,
            None,
        ));
    };
    serde_json::from_value(params).map_err(|e| {
        varlink::Error(
            varlink::ErrorKind::InvalidParameter(e.to_string()),
            None,
            None,
        )
    })
}

#[async_trait]
impl varlink::AsyncConnectionHandler for StreamingHandler {
    async fn handle(
        &self,
        server: &mut varlink::sansio::Server,
        _upgraded_iface: Option<String>,
    ) -> varlink::Result<Option<String>> {
        while let Some(event) = server.poll_event() {
            match event {
                varlink::sansio::ServerEvent::Request { request } => {
                    let more = request.more.unwrap_or(false);
                    let oneway = request.oneway.unwrap_or(false);

                    if request.method == "dev.prefix.pixi.ConfirmGlobalInstall" && more {
                        let args: ConfirmGlobalInstall_Args =
                            parse_args(request.parameters)?;
                        self.handle_confirm_streaming(server, args).await?;
                    } else {
                        let mut call = AsyncCall::new(more, oneway);
                        self.dispatch(&mut call, &request).await?;
                        for reply in call.take_replies() {
                            server.send_reply(reply)?;
                        }
                    }
                }
                varlink::sansio::ServerEvent::Upgrade { interface } => {
                    return Ok(Some(interface));
                }
            }
        }
        Ok(None)
    }
}

impl StreamingHandler {
    async fn dispatch(
        &self,
        call: &mut AsyncCall,
        request: &varlink::Request<'_>,
    ) -> varlink::Result<()> {
        match request.method.as_ref() {
            "dev.prefix.pixi.ConfirmGlobalInstall" => {
                let args: ConfirmGlobalInstall_Args =
                    parse_args(request.parameters.clone())?;
                self.inner
                    .confirm_global_install(call, args.challenge)
                    .await
            }
            "dev.prefix.pixi.GlobalInstall" => {
                let args: GlobalInstall_Args = parse_args(request.parameters.clone())?;
                self.inner
                    .global_install(
                        call,
                        args.packages,
                        args.channels,
                        args.platform,
                        args.environment,
                        args.expose,
                        args.with,
                        args.force_reinstall,
                        args.no_shortcuts,
                        args.client_envs_dir,
                    )
                    .await
            }
            "dev.prefix.pixi.Info" => {
                let args: Info_Args = parse_args(request.parameters.clone())?;
                self.inner.info(call, args.manifest_path).await
            }
            method => {
                use varlink::CallTrait;
                call.reply_method_not_found(method.to_string())?;
                Ok(())
            }
        }
    }

    async fn handle_confirm_streaming(
        &self,
        server: &mut varlink::sansio::Server,
        args: ConfirmGlobalInstall_Args,
    ) -> varlink::Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        // Run the install (which uses the reporter that sends to tx)
        // on a blocking-friendly spawned task, while we drain progress
        // messages on this task.
        let inner = self.inner.clone();
        let challenge = args.challenge;

        let install_future = tokio::spawn(async move {
            inner.confirm_global_install_streaming(challenge, tx).await
        });

        // Pin the future so we can poll it in select!
        tokio::pin!(install_future);

        loop {
            tokio::select! {
                biased;

                msg = rx.recv() => {
                    match msg {
                        Some(message) => {
                            let reply = varlink::Reply {
                                parameters: Some(json!({ "message": message, "sha": null, "sha_dir": null, "packages": null })),
                                continues: Some(true),
                                error: None,
                            };
                            server.send_reply(reply)?;
                        }
                        None => {
                            // Channel closed — install finished, drain the join handle
                            let result = (&mut install_future).await.map_err(|e| {
                                varlink::Error(
                                    varlink::ErrorKind::InvalidParameter(e.to_string()),
                                    None,
                                    None,
                                )
                            })?;
                            // Send the final replies
                            for reply in result? {
                                server.send_reply(reply)?;
                            }
                            break;
                        }
                    }
                }
                result = &mut install_future => {
                    // Install finished before channel drained — drain remaining
                    while let Ok(message) = rx.try_recv() {
                        let reply = varlink::Reply {
                            parameters: Some(json!({ "message": message, "sha": null, "sha_dir": null, "packages": null })),
                            continues: Some(true),
                            error: None,
                        };
                        server.send_reply(reply)?;
                    }
                    let result = result.map_err(|e| {
                        varlink::Error(
                            varlink::ErrorKind::InvalidParameter(e.to_string()),
                            None,
                            None,
                        )
                    })?;
                    for reply in result? {
                        server.send_reply(reply)?;
                    }
                    break;
                }
            }
        }

        Ok(())
    }
}
