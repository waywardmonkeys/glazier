// Copyright 2019 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! macOS implementation of features at the application scope.

#![allow(non_upper_case_globals)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use cocoa::appkit::{NSApp, NSApplication, NSApplicationActivationPolicyRegular};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSArray, NSAutoreleasePool};
use lazy_static::lazy_static;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use crate::application::AppHandler;
use crate::common_util::{shared_queue, SharedDequeuer, SharedEnqueuer};

use super::clipboard::Clipboard;
use super::error::Error;
use super::util;

static APP_DELEGATE_STATE_IVAR: &str = "glazierDelegateState";

#[derive(Clone)]
pub(crate) struct Application {
    ns_app: id,
    delegate: id,
    state: Rc<RefCell<State>>,
}

struct State {
    quitting: bool,
}

impl Application {
    pub fn new() -> Result<Application, Error> {
        // macOS demands that we run not just on one thread,
        // but specifically the first thread of the app.
        util::assert_main_thread();
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let ns_app = NSApp();
            let state = Rc::new(RefCell::new(State { quitting: false }));

            // Initialize the application delegate
            let delegate: id = msg_send![APP_DELEGATE.0, alloc];
            let () = msg_send![delegate, init];
            let delegate_state = DelegateState {
                handler: None,
                run_on_main_queue: shared_queue(),
            };
            let delegate_state_ptr = Box::into_raw(Box::new(delegate_state));
            (*delegate).set_ivar(APP_DELEGATE_STATE_IVAR, delegate_state_ptr as *mut c_void);
            let () = msg_send![ns_app, setDelegate: delegate];

            Ok(Application {
                ns_app,
                delegate,
                state,
            })
        }
    }

    pub fn run(self, handler: Option<Box<dyn AppHandler>>) {
        unsafe {
            let state_ptr = *(*self.delegate).get_ivar::<*mut c_void>(APP_DELEGATE_STATE_IVAR)
                as *mut DelegateState;
            state_ptr.as_mut().expect("delegate state").handler = handler;

            // Run the main app loop
            self.ns_app.run();

            // Clean up the delegate
            let () = msg_send![self.ns_app, setDelegate: nil];
            drop(Box::from_raw(state_ptr)); // Causes it to drop & dealloc automatically
        }
    }

    pub fn quit(&self) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            if !state.quitting {
                state.quitting = true;
                unsafe {
                    // We want to queue up the destruction of all our windows.
                    // Failure to do so will lead to resource leaks.
                    let windows: id = msg_send![self.ns_app, windows];
                    for i in 0..windows.count() {
                        let window: id = windows.objectAtIndex(i);
                        let () = msg_send![window, performSelectorOnMainThread: sel!(close) withObject: nil waitUntilDone: NO];
                    }
                    // Stop sets a stop request flag in the OS.
                    // The run loop is stopped after dealing with events.
                    let () = msg_send![self.ns_app, stop: nil];
                }
            }
        } else {
            tracing::warn!("Application state already borrowed");
        }
    }

    pub fn clipboard(&self) -> Clipboard {
        Clipboard
    }

    pub fn get_locale() -> String {
        unsafe {
            let nslocale_class = class!(NSLocale);
            let locale: id = msg_send![nslocale_class, currentLocale];
            let ident: id = msg_send![locale, localeIdentifier];
            let mut locale = util::from_nsstring(ident);
            // This is done because the locale parsing library we use expects an unicode locale, but these vars have an ISO locale
            if let Some(idx) = locale.chars().position(|c| c == '@') {
                locale.truncate(idx);
            }
            locale
        }
    }

    pub fn get_handle(&self) -> Option<AppHandle> {
        let delegate = unsafe { DelegateState::from_delegate(&mut *self.delegate) };

        Some(AppHandle {
            enqueuer: delegate.run_on_main_queue.0.clone(),
        })
    }
}

impl crate::platform::mac::ApplicationExt for crate::Application {
    fn hide(&self) {
        unsafe {
            let () = msg_send![self.backend_app.ns_app, hide: nil];
        }
    }

    fn hide_others(&self) {
        unsafe {
            let workspace = class!(NSWorkspace);
            let shared: id = msg_send![workspace, sharedWorkspace];
            let () = msg_send![shared, hideOtherApplications];
        }
    }

    fn set_menu(&self, menu: crate::Menu) {
        unsafe {
            NSApp().setMainMenu_(menu.0.menu);
        }
    }
}

type MainThreadCb = Box<dyn FnOnce(Option<&mut dyn AppHandler>) + Send>;

#[derive(Clone)]
pub(crate) struct AppHandle {
    enqueuer: SharedEnqueuer<MainThreadCb>,
}

impl AppHandle {
    pub fn run_on_main<F>(&self, callback: F)
    where
        F: FnOnce(Option<&mut dyn AppHandler>) + Send + 'static,
    {
        let needs_wake = self.enqueuer.enqueue(Box::new(callback));

        if needs_wake {
            unsafe {
                let nsapp = NSApp();
                let delegate: id = msg_send![nsapp, delegate];
                let () = msg_send![delegate,
                    performSelectorOnMainThread: sel!(runOnMainQueue)
                    withObject: nil
                    waitUntilDone: NO];
            }
        }
    }
}

struct DelegateState {
    handler: Option<Box<dyn AppHandler>>,
    run_on_main_queue: (SharedEnqueuer<MainThreadCb>, SharedDequeuer<MainThreadCb>),
}

impl DelegateState {
    unsafe fn from_delegate(delegate: &mut Object) -> &mut Self {
        let state: *mut c_void = *delegate.get_ivar(APP_DELEGATE_STATE_IVAR);
        &mut *(state as *mut DelegateState)
    }

    fn command(&mut self, command: u32) {
        if let Some(inner) = self.handler.as_mut() {
            inner.command(command)
        }
    }
}

struct AppDelegate(*const Class);
unsafe impl Sync for AppDelegate {}
unsafe impl Send for AppDelegate {}

lazy_static! {
    static ref APP_DELEGATE: AppDelegate = unsafe {
        let mut decl = ClassDecl::new("DruidAppDelegate", class!(NSObject))
            .expect("App Delegate definition failed");
        decl.add_ivar::<*mut c_void>(APP_DELEGATE_STATE_IVAR);

        decl.add_method(
            sel!(applicationDidFinishLaunching:),
            application_did_finish_launching as extern "C" fn(&mut Object, Sel, id),
        );

        decl.add_method(
            sel!(handleMenuItem:),
            handle_menu_item as extern "C" fn(&mut Object, Sel, id),
        );

        decl.add_method(
            sel!(runOnMainQueue),
            run_on_main_queue as extern "C" fn(&mut Object, Sel),
        );

        AppDelegate(decl.register())
    };
}

extern "C" fn application_did_finish_launching(_this: &mut Object, _: Sel, _notification: id) {
    unsafe {
        let ns_app = NSApp();
        // We need to delay setting the activation policy and activating the app
        // until we have the main menu all set up. Otherwise the menu won't be interactable.
        ns_app.setActivationPolicy_(NSApplicationActivationPolicyRegular);
        let () = msg_send![ns_app, activateIgnoringOtherApps: YES];
    }
}

/// This handles menu items in the case that all windows are closed.
extern "C" fn handle_menu_item(this: &mut Object, _: Sel, item: id) {
    unsafe {
        let tag: isize = msg_send![item, tag];
        let state = DelegateState::from_delegate(this);
        state.command(tag as u32);
    }
}

extern "C" fn run_on_main_queue(this: &mut Object, _: Sel) {
    unsafe {
        let state = DelegateState::from_delegate(this);
        for cb in &mut state.run_on_main_queue.1 {
            cb(match state.handler.as_mut() {
                Some(handler) => Some(handler.as_mut()),
                None => None,
            });
        }
    }
}
