#![feature(iter_collect_into)]
#![allow(non_snake_case)]
#![feature(mem_copy_fn)]
#![feature(macro_metavar_expr)]
#![feature(slice_take)]
#![allow(clippy::multiple_crate_versions, clippy::future_not_send)]
#![allow(clippy::empty_docs)]

use {
    crate::{
        chat::Chat,
        login::{Login, Register},
        profile::Profile,
    },
    anyhow::Context,
    chat_client_node::{
        BootPhase, ChatMeta, HardenedChatInvitePayload, JoinRequestPayload, MailVariants,
        MemberMeta, Node, RequestContext, Requests, UserKeys, Vault,
    },
    chat_spec::{ChatName, Nonce, Proof, UserName},
    codec::{Codec, Reminder},
    double_ratchet::DoubleRatchet,
    leptos::{
        component, create_effect, create_node_ref,
        html::{self, Input},
        leptos_dom::helpers::TimeoutHandle,
        mount_to_body, provide_context, set_timeout, set_timeout_with_handle,
        signal_prelude::*,
        spawn_local, store_value, use_context, view, CollectView, IntoView, NodeRef, StoredValue,
    },
    leptos_router::{Route, Router, Routes, A},
    libp2p::futures::{future::join_all, FutureExt, StreamExt, TryFutureExt},
    rand::rngs::OsRng,
    std::{cmp::Ordering, convert::identity, fmt::Display, future::Future, time::Duration},
    web_sys::wasm_bindgen::JsValue,
};

mod chat;
mod db;
mod login;
mod profile;

pub fn main() {
    console_error_panic_hook::set_once();
    _ = console_log::init_with_level(if cfg!(debug_assertions) {
        log::Level::Debug
    } else {
        log::Level::Error
    });
    mount_to_body(App)
}

#[derive(Default, Clone, Copy)]
struct State {
    keys: RwSignal<Option<UserKeys>>,
    requests: StoredValue<Option<Requests>>,
    vault: RwSignal<Vault>,
    vault_version: StoredValue<Nonce>,
    mail_action: StoredValue<Nonce>,
    hardened_messages: RwSignal<Option<(ChatName, db::Message)>>,
}

type Result<T> = anyhow::Result<T>;

// TODO: emmit errors instead of unwraps
impl RequestContext for State {
    fn with_vault<R>(&self, action: impl FnOnce(&mut Vault) -> Result<R>) -> Result<R> {
        self.vault
            .try_update(|vault| action(vault))
            .context("vault not available, might need reload")
            .and_then(identity)
    }

    fn with_keys<R>(&self, action: impl FnOnce(&UserKeys) -> R) -> Result<R> {
        self.keys
            .try_with_untracked(|keys| keys.as_ref().map(action))
            .flatten()
            .context("keys not available, might need reload")
    }

    fn with_vault_version<R>(&self, action: impl FnOnce(&mut Nonce) -> R) -> Result<R> {
        self.vault_version
            .try_update_value(action)
            .context("vault version not available, might need reload")
    }

    fn with_mail_action<R>(&self, action: impl FnOnce(&mut Nonce) -> R) -> Result<R> {
        self.mail_action
            .try_update_value(action)
            .context("mail action not available, might need reload")
    }
}

impl State {
    fn set_chat_nonce(&self, chat: ChatName, nonce: Nonce) {
        self.vault.update(|v| {
            let chat = v.chats.get_mut(&chat).expect("chat not found");
            chat.action_no = nonce;
        });
    }

    fn next_chat_proof(self, chat_name: ChatName) -> Option<chat_spec::Proof<ChatName>> {
        self.keys
            .try_with_untracked(|keys| {
                let keys = keys.as_ref()?;
                self.vault.try_update(|vault| {
                    let chat = vault.chats.get_mut(&chat_name)?;
                    Some(Proof::new(&keys.sign, &mut chat.action_no, chat_name, OsRng))
                })
            })
            .flatten()
            .flatten()
    }

    fn next_profile_proof(self, vault: &[u8]) -> Option<chat_spec::Proof<Reminder>> {
        self.keys
            .try_with_untracked(|keys| {
                let keys = keys.as_ref()?;
                self.vault_version.try_update_value(|nonce| {
                    Some(Proof::new(&keys.sign, nonce, Reminder(vault), OsRng))
                })
            })
            .flatten()
            .flatten()
    }

    fn chat_secret(self, chat_name: ChatName) -> Option<crypto::SharedSecret> {
        self.vault.with_untracked(|vault| vault.chats.get(&chat_name).map(|c| c.secret))
    }
}

fn App() -> impl IntoView {
    let (rboot_phase, wboot_phase) = create_signal(None::<BootPhase>);
    let (errors, set_errors) = create_signal(None::<anyhow::Error>);
    provide_context(Errors(set_errors));
    let state = State::default();

    let serialized_vault = create_memo(move |_| {
        log::debug!("vault serialized");
        state.vault.with(Codec::to_bytes)
    });
    let timeout = store_value(None::<TimeoutHandle>);
    let save_vault = move |keys: UserKeys, mut ed: Requests| {
        let mut vault_bytes = serialized_vault.get_untracked();
        crate::encrypt(&mut vault_bytes, keys.vault);
        handled_spawn_local("saving vault", async move {
            let proof = state.next_profile_proof(&vault_bytes).expect("logged in");
            ed.set_vault(proof).await.context("setting vault")?;
            log::debug!("saved vault");
            Ok(())
        });
    };
    create_effect(move |init| {
        serialized_vault.track();
        _ = init?;
        let keys = state.keys.get_untracked()?;
        let ed = state.requests.get_value()?;

        if let Some(handle) = timeout.get_value() {
            handle.clear()
        }
        let handle =
            set_timeout_with_handle(move || save_vault(keys, ed), Duration::from_secs(3)).unwrap();
        timeout.set_value(Some(handle));

        Some(())
    });

    let handle_mail = move |mut raw_mail: &[u8], new_messages: &mut Vec<db::Message>| {
        let Some(mail) = MailVariants::decode(&mut raw_mail) else {
            anyhow::bail!("faild to decode mail, {raw_mail:?}");
        };

        match mail {
            MailVariants::ChatInvite { chat, cp } => {
                let secret = state
                    .with_keys(|k| k.enc.decapsulate_choosen(&cp))?
                    .context("failed to decapsulate invite")?;
                state.vault.update(|v| _ = v.chats.insert(chat, ChatMeta::from_secret(secret)));
            }
            MailVariants::HardenedJoinRequest { cp, mut payload } => {
                let secret = state
                    .with_keys(|k| k.enc.decapsulate(&cp))?
                    .context("failed to decapsulate invite")?;

                let payload = crypto::decrypt(&mut payload, secret)
                    .context("failed to decrypt join request payload")?;

                let JoinRequestPayload { chat, name, identity, init_pk } =
                    <_>::decode(&mut &*payload).context("failed to decode join request payload")?;

                let dr = DoubleRatchet::recipient(init_pk, secret, OsRng);

                if state.vault.with_untracked(|v| !v.hardened_chats.contains_key(&chat)) {
                    anyhow::bail!("request to join chat we dont have");
                }

                state.vault.update(|v| {
                    log::debug!("adding member to hardened chat");
                    let chat = v.hardened_chats.get_mut(&chat).expect("we just checked");
                    chat.members.insert(name, MemberMeta { dr, identity });
                })
            }
            MailVariants::HardenedChatInvite { cp, mut payload } => {
                log::debug!("handling hardened chat invite");
                let secret = state
                    .with_keys(|k| k.enc.decapsulate(&cp))?
                    .context("failed to decapsulate hardened invite")?;

                let payload = crypto::decrypt(&mut payload, secret)
                    .context("failed to decrypt hardened invite")?;
                let HardenedChatInvitePayload { chat, inviter, inviter_id, members, init_pk } =
                    <_>::decode(&mut &*payload).context("failed to decode hardened invite")?;

                let dr = DoubleRatchet::recipient(init_pk, secret, OsRng);

                handled_spawn_local("inviting hardened user user", async move {
                    let dispatch = state.requests.get_value().context("no dispatch")?;
                    let members = join_all(members.into_iter().map(|(name, id)| {
                        dispatch
                            .clone()
                            .send_hardened_join_request(id, chat, state)
                            .map_ok(move |dr| (name, dr))
                    }))
                    .await
                    .into_iter()
                    .collect::<anyhow::Result<Vec<_>>>()?;

                    state.vault.update(|v| {
                        let meta = v.hardened_chats.entry(chat).or_default();
                        members
                            .into_iter()
                            .for_each(|(name, secret)| _ = meta.members.insert(name, secret));
                        meta.members.insert(inviter, MemberMeta { dr, identity: inviter_id });
                    });

                    Ok(())
                });
            }
            MailVariants::HardenedChatMessage { nonce, chat, header, mut content } => {
                state.with_vault(|v| {
                    let Some((&chat, meta)) = v
                        .hardened_chats
                        .iter_mut()
                        .find(|(c, _)| crypto::hash::with_nonce(c.as_bytes(), nonce) == chat)
                    else {
                        log::warn!("received message for unknown chat");
                        return Ok(());
                    };

                    let Some((&sender, content)) =
                        meta.members.iter_mut().find_map(|(name, mm)| {
                            let secret = mm.dr.recv_message(header, OsRng)?;
                            let decrypted = crypto::decrypt(&mut content, secret)?;
                            Some((name, std::str::from_utf8(decrypted).ok()?.into()))
                        })
                    else {
                        log::warn!("received message for chat we are not in");
                        return Ok(());
                    };

                    let message =
                        db::Message { sender, owner: state.with_keys(|k| k.name)?, content, chat };
                    new_messages.push(message.clone());
                    state.hardened_messages.set(Some((chat, message)));

                    Ok(())
                })?;
            }
        }

        Ok(())
    };

    create_effect(move |_| {
        let Some(keys) = state.keys.get() else {
            return;
        };

        handled_spawn_local("initializing node", async move {
            navigate_to("/");
            let (node, vault, dispatch, vault_version, mail_action) =
                Node::new(keys, |s| wboot_phase(Some(s)))
                    .await
                    .inspect_err(|_| navigate_to("/login"))?;

            let mut dispatch_clone = dispatch.clone();
            handled_spawn_local("reading mail", async move {
                let Reminder(list) = dispatch_clone.read_mail(state).await?;
                let mut new_messages = Vec::new();
                for mail in chat_spec::unpack_mail(list) {
                    handle_error(handle_mail(mail, &mut new_messages).context("receiving a mail"));
                }
                db::save_messages(new_messages).await
            });

            let mut dispatch_clone = dispatch.clone();
            let listen = async move {
                let identity = state.with_keys(UserKeys::identity_hash)?;
                let mut account =
                    dispatch_clone.subscribe(identity).await.context("subscribing to profile")?;
                while let Some(mail) = account.next().await {
                    let mut new_messages = Vec::new();
                    handle_error(handle_mail(&mail, &mut new_messages).context("receiving a mail"));
                    handle_error(db::save_messages(new_messages).await);
                }

                anyhow::Result::Ok(())
            };

            state.requests.set_value(Some(dispatch));
            state.vault.set_untracked(vault);
            state.vault_version.set_value(vault_version);
            state.mail_action.set_value(mail_action);
            navigate_to("/chat");

            let res = libp2p::futures::select! {
                _ = node.fuse() => Err(anyhow::anyhow!("local node terminated")),
                e = listen.fuse() => e,
            };
            navigate_to("/login");
            res
        });
    });

    let chat = move || view! { <Chat state/> };
    let profile = move || view! { <Profile state/> };
    let login = move || view! { <Login state/> };
    let register = move || view! { <Register state/> };
    let boot = move || view! { <Boot rboot_phase/> };

    view! {
        <Router>
        <Routes>
            <Route path="/chat/:id?" view=chat></Route>
            <Route path="/profile" view=profile></Route>
            <Route path="/login" view=login></Route>
            <Route path="/register" view=register></Route>
            <Route path="/" view=boot></Route>
        </Routes>
        </Router>
        <ErrorPanel errors/>
    }
}

#[component]
fn ErrorPanel(errors: ReadSignal<Option<anyhow::Error>>) -> impl IntoView {
    let error_nodes = create_node_ref::<html::Div>();
    let error_message = move |message: String| {
        let elem = view! {
            <div class="tbm" onclick="this.remove()">
                <div class="ec hov bp pea" style="cursor: pointer">{message}</div>
            </div>
        };

        let celem = elem.clone();
        set_timeout(move || celem.remove(), Duration::from_secs(7));

        elem
    };

    create_effect(move |_| {
        errors.with(|e| {
            if let Some(e) = e {
                log::error!("{e:#}");
                error_nodes
                    .get_untracked()
                    .unwrap()
                    .append_child(&error_message(format!("{e:#}")))
                    .unwrap();
            }
        });
    });

    view! {
        <div class="fsc jcfe flx pen">
            <div class="bm" style="align-self: flex-end;" node_ref=error_nodes />
        </div>
    }
}

#[component]
fn Boot(rboot_phase: ReadSignal<Option<BootPhase>>) -> impl IntoView {
    let phases = (0..BootPhase::ChatRun.discriminant())
        .map(|i| {
            let margin = if i == 0 { "" } else { "lbm" };
            let compute_class = move || {
                rboot_phase.with(|phase| match phase.map(|p| i.cmp(&(p.discriminant()))) {
                    Some(Ordering::Less) => "bar-loaded",
                    Some(Ordering::Equal) => "bar-loading",
                    Some(Ordering::Greater) => "bar-unloaded",
                    None => "",
                })
            };
            view! { <span class=move || format!("bp hc fg1 tac {} {margin}", compute_class()) /> }
        })
        .collect_view();

    let message = move || match rboot_phase() {
        Some(s) => format!("{s}"),
        None => {
            navigate_to("/login");
            "confused".to_string()
        }
    };

    view! {
        <main class="ma sc bp">
            <h1>Initiating connections</h1>
            <p>{message}</p>
            <div class="flx bp pc">
                {phases}
            </div>
        </main>
    }
}

#[component]
fn Nav(my_name: UserName) -> impl IntoView {
    let menu = create_node_ref::<html::Div>();
    let on_menu_toggle = move |_| {
        let menu = menu.get_untracked().unwrap();
        menu.set_hidden(!menu.hidden());
    };

    let uname = move || my_name.to_string();

    view! {
        <nav class="sc flx fdc fg0 phone-only">
            <div class="flx jcsb">
                <button class="rsb hov sc nav-menu-button" on:click=on_menu_toggle>/menu</button>
                <div class="bp bf hc lsb">{uname}</div>
            </div>
            <div class="flx fdc tsm" hidden node_ref=menu>
                <A class="bf hov bp sc bsb" href="/chat">/rooms</A>
                <A class="bf hov bp sc sb" href="/profile">/profile</A>
                <A class="bf hov bp sc tsb" href="/login">/logout</A>
            </div>
        </nav>

        <nav class="sc flx jcsb fg0 desktop-only">
            <div class="flx">
                <A class="bf hov bp sc rsb" href="/chat">/rooms</A>
                <A class="bf hov bp sc sb" href="/profile">/profile</A>
                <A class="bf hov bp sc sb" href="/login">/logout</A>
            </div>
            <div class="bp bf hc lsb">{uname}</div>
        </nav>
    }
}

fn get_value(elem: NodeRef<Input>) -> String {
    elem.get_untracked().unwrap().value()
}

fn navigate_to(path: impl Display) {
    leptos_router::use_navigate()(&format!("{path}"), Default::default());
}

fn not(signal: impl Fn() -> bool + Copy) -> impl Fn() -> bool + Copy {
    move || !signal()
}

fn handle_js_err(jv: JsValue) -> anyhow::Error {
    anyhow::anyhow!("{jv:?}")
}

#[derive(Clone, Copy)]
struct Errors(WriteSignal<Option<anyhow::Error>>);

fn handle_error<T>(r: anyhow::Result<T>) -> Option<T> {
    let Errors(errors) = use_context().unwrap();
    r.map_err(|e| errors.set(Some(e))).ok()
}

fn _handled_closure(
    context: &'static str,
    f: impl Fn() -> anyhow::Result<()> + 'static + Copy,
) -> impl Fn() + 'static + Copy {
    let Errors(errors) = use_context().unwrap();
    move || {
        if let Err(e) = f().context(context) {
            errors.set(Some(e));
        }
    }
}

fn handled_callback<T>(
    context: &'static str,
    f: impl Fn(T) -> anyhow::Result<()> + 'static + Copy,
) -> impl Fn(T) + 'static + Copy {
    let Errors(errors) = use_context().unwrap();
    move |v| {
        if let Err(e) = f(v).context(context) {
            errors.set(Some(e));
        }
    }
}

fn handled_async_closure<F: Future<Output = anyhow::Result<()>> + 'static>(
    context: &'static str,
    f: impl Fn() -> F + 'static + Copy,
) -> impl Fn() + 'static + Copy {
    let Errors(errors) = use_context().unwrap();
    move || {
        let fut = f();
        spawn_local(async move {
            if let Err(e) = fut.await.context(context) {
                errors.set(Some(e));
            }
        });
    }
}

fn handled_async_callback<F: Future<Output = anyhow::Result<()>> + 'static, T>(
    context: &'static str,
    f: impl Fn(T) -> F + 'static + Copy,
) -> impl Fn(T) + 'static + Copy {
    let Errors(errors) = use_context().unwrap();
    move |v| {
        let fut = f(v);
        spawn_local(async move {
            if let Err(e) = fut.await.context(context) {
                errors.set(Some(e));
            }
        });
    }
}

fn handled_spawn_local(
    context: &'static str,
    f: impl Future<Output = anyhow::Result<()>> + 'static,
) {
    let Errors(errors) = use_context().unwrap();
    spawn_local(async move {
        if let Err(e) = f.await.context(context) {
            errors.set(Some(e));
        }
    });
}

fn encrypt(data: &mut Vec<u8>, secret: crypto::SharedSecret) {
    let arr = crypto::encrypt(data, secret, OsRng);
    data.extend(arr);
}