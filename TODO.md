
    #[allow(dead_code, unused)]
    fn connect_focus_out_event(ctx: Rc<RefCell<Ctx>>) {
        let window = ctx.borrow()._window.clone().expect("missing window");
        window.connect_focus_out_event(move |_, _| {
            let it = ctx.borrow().cfg.on_focus_out;
            match it {
                FocusOutBehavior::Hide => {
                    ctx.borrow_mut().toggle();
                }
                FocusOutBehavior::Refocus => {
                    // todo
                }
                FocusOutBehavior::None => {}
            }

            return Propagation::Proceed;
        });
    }
    
