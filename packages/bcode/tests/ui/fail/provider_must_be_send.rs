use bcode::{
    InProcessModelProvider, InProcessProviderContext, InProcessProviderFuture, ModelTurnRequest,
};
use std::rc::Rc;

struct NotSendProvider(Rc<()>);

impl InProcessModelProvider for NotSendProvider {
    fn run_turn(
        &self,
        _request: ModelTurnRequest,
        _context: InProcessProviderContext,
    ) -> InProcessProviderFuture<'_> {
        unimplemented!()
    }
}

fn main() {}
