use crate::echo_capnp;

#[derive(Debug)]
pub struct IpcInterface;

impl echo_capnp::echo::Server for IpcInterface {
    async fn echo(
        self: capnp::capability::Rc<Self>,
        params: echo_capnp::echo::EchoParams,
        mut results: echo_capnp::echo::EchoResults,
    ) -> Result<(), capnp::Error> {
        let request = params.get()?.get_msg()?;
        let msg = request.to_string()?;
        results.get().set_reply(msg);
        Ok(())
    }
}
