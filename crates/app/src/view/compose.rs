//! 写信框：写新邮件、回复、转发、接着写草稿。关掉时内容改过就存进草稿文件夹，发出或丢弃时删掉接着写的旧草稿

use super::*;
use gpui_component::input::{Position, Textarea, TextareaState};
use mailpro_core::message;
use mailpro_core::smtp::{self, Built, Mail, join};

/// 阅读区对正在看的这封的回应
#[derive(Clone, Copy)]
pub(super) enum Respond {
    Reply,
    ReplyAll,
    Forward,
}

pub(super) struct Compose {
    /// 用哪个账号发
    account: i64,
    /// 可选的发件账号：（id，“名字 <地址>”）
    senders: Vec<(i64, String)>,
    to: Entity<InputState>,
    cc: Entity<InputState>,
    bcc: Entity<InputState>,
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
    /// 展开了抄送、密送
    more: bool,
    in_reply_to: Option<String>,
    references: Vec<String>,
    /// 回复的原信（账号，文件夹，uid），发出后标上已回复
    answers: Option<(i64, i64, u32)>,
    /// 接着写的草稿（账号，文件夹，uid）
    draft: Option<(i64, i64, u32)>,
    /// 打开时的账号和内容，关掉时没改过就不存草稿
    initial: (i64, Mail),
    title: &'static str,
    /// 正在发送 / 存草稿，期间按钮置灰、关不掉
    busy: Option<&'static str>,
    error: Option<String>,
}

impl Compose {
    fn mail(&self, cx: &App) -> Mail {
        let text = |e: &Entity<InputState>| e.read(cx).value().to_string();
        Mail {
            to: text(&self.to),
            cc: text(&self.cc),
            bcc: text(&self.bcc),
            subject: text(&self.subject),
            body: self.body.read(cx).value().to_string(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
        }
    }
}

impl MailPro {
    /// 写新邮件：用正在看的那封所属的账号发，否则用选中文件夹的
    pub(super) fn compose(&mut self, mail: Mail, window: &mut Window, cx: &mut Context<Self>) {
        let account = self.reading.as_ref().map(|r| r.account).filter(|&a| a != 0).or(self.sel.map(|s| s.0)).or(self.accounts.first().map(|a| a.acc.id));
        if let Some(account) = account {
            self.open_compose(account, t(cx).compose, mail, None, None, window, cx);
        }
    }

    /// 回复、全部回复或转发正在看的这封
    pub(super) fn respond(&mut self, kind: Respond, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        let Some(r) = &self.reading else { return };
        let (account, folder, uid) = (r.account, r.folder, r.uid);
        let Some(o) = self.db.body(folder, uid).ok().flatten().and_then(|raw| message::original(&raw)) else { return };
        let Some(me) = self.accounts.iter().find(|a| a.acc.id == account).map(|a| a.acc.email.clone()) else { return };
        // 引文里的时间也用 UTC，和发信的 Date 一样不暴露时区
        let date = DateTime::from_timestamp(o.date.unwrap_or(r.date), 0).unwrap_or_default().format("%Y-%m-%d %H:%M UTC").to_string();
        let from = join(&o.from);
        let (title, mail) = match kind {
            Respond::Reply | Respond::ReplyAll => {
                let all = matches!(kind, Respond::ReplyAll);
                let (to, cc) = o.recipients(all, &me);
                let (in_reply_to, references) = o.thread();
                let wrote = t.wrote.replace("{date}", &date).replace("{from}", &from);
                let body = format!("\n\n{wrote}\n{}", message::quote(&o.text));
                let subject = message::reply_subject(&o.subject);
                (if all { t.reply_all } else { t.reply }, Mail { to: join(&to), cc: join(&cc), subject, body, in_reply_to, references, ..Default::default() })
            }
            // 转发只带原文，不带附件
            Respond::Forward => {
                let line = |label: &str, value: String| (!value.is_empty()).then(|| format!("{label}{}{value}", t.colon));
                let head = [line(t.from, from), line(t.date, date), line(t.subject, o.subject.clone()), line(t.to, join(&o.to)), line(t.cc, join(&o.cc))];
                let body = format!("\n\n{}\n{}\n\n{}", t.forwarded, head.into_iter().flatten().collect::<Vec<_>>().join("\n"), o.text);
                (t.forward, Mail { subject: message::forward_subject(&o.subject), body, ..Default::default() })
            }
        };
        let answers = (!matches!(kind, Respond::Forward)).then_some((account, folder, uid));
        self.open_compose(account, title, mail, answers, None, window, cx);
    }

    /// 接着写草稿文件夹里的这封
    pub(super) fn edit_draft(&mut self, account: i64, folder: i64, uid: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.db.body(folder, uid).ok().flatten().and_then(|raw| message::original(&raw)) else { return };
        let mail = Mail { to: join(&o.to), cc: join(&o.cc), bcc: join(&o.bcc), subject: o.subject, body: o.text, in_reply_to: o.in_reply_to, references: o.references };
        self.open_compose(account, t(cx).compose, mail, None, Some((account, folder, uid)), window, cx);
    }

    fn open_compose(&mut self, account: i64, title: &'static str, mail: Mail, answers: Option<(i64, i64, u32)>, draft: Option<(i64, i64, u32)>, window: &mut Window, cx: &mut Context<Self>) {
        let mut input = |value: &str| {
            let value = value.to_string();
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let (to, cc, bcc, subject) = (input(&mail.to), input(&mail.cc), input(&mail.bcc), input(&mail.subject));
        let body = cx.new(|cx| TextareaState::new(window, cx).default_value(mail.body.clone()));
        let senders = self.accounts.iter().map(|a| (a.acc.id, sender(&a.acc))).collect();
        let more = !mail.cc.is_empty() || !mail.bcc.is_empty();
        let (in_reply_to, references) = (mail.in_reply_to, mail.references);
        let initial = (account, Mail::default());
        let mut c = Compose { account, senders, to, cc, bcc, subject, body, more, in_reply_to, references, answers, draft, initial, title, busy: None, error: None };
        c.initial.1 = c.mail(cx);
        let c = cx.new(|_| c);
        self.compose = Some(c.clone());
        let view = cx.entity().downgrade();
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // 头一次画时定焦点：没有收件人先填收件人，否则从正文开头写
            if first.replace(false) {
                let (to, body) = (c.read(cx).to.clone(), c.read(cx).body.clone());
                if to.read(cx).value().is_empty() {
                    to.update(cx, |s, cx| s.focus(window, cx));
                } else {
                    body.update(cx, |s, cx| s.set_cursor_position(Position::new(0, 0), window, cx));
                }
            }
            compose_dialog(&c, &view, dialog, cx)
        });
    }

    /// 关写信框（Esc、右上角关闭）：发送、存草稿中关不掉；内容没改过直接关，改过先存草稿，存好再关
    fn close_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(c) = &self.compose else { return true };
        let c = c.read(cx);
        if c.busy.is_some() {
            return false;
        }
        if (c.account, c.mail(cx)) == c.initial {
            self.compose = None;
            return true;
        }
        self.save_draft(window, cx);
        false
    }

    /// 用写信框选的账号拼信；账号没连上或地址有误时返回原因
    fn built(&self, c: &Compose, cx: &App) -> Result<(i64, Built), String> {
        let t = t(cx);
        let Some(a) = self.accounts.iter().find(|a| a.acc.id == c.account) else { return Err(t.offline.into()) };
        if a.error.is_some() {
            return Err(t.offline.into());
        }
        smtp::build(&a.acc, &c.mail(cx)).map(|b| (a.acc.id, b)).map_err(|item| format!("{}{item}", t.bad_address))
    }

    fn send_mail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        let Some(c) = self.compose.clone() else { return };
        let r = self.built(c.read(cx), cx).and_then(|x| if x.1.envelope.is_some() { Ok(x) } else { Err(t.no_recipients.into()) });
        let (account, mail) = match r {
            Ok(x) => x,
            Err(e) => return status(&c, None, Some(e), cx),
        };
        // Gmail 发出的自己会存进已发送；没有已发送文件夹就不存
        let saves = self.accounts.iter().any(|a| a.acc.id == account && smtp::saves_sent(&a.acc));
        let sent = if saves { None } else { self.role_folder(account, "sent") };
        let (reply, rx) = oneshot::channel();
        let _ = self.cmd.unbounded_send(Cmd::Send { account, mail, sent, reply });
        status(&c, Some(t.sending), None, cx);
        self.wait(rx, true, window, cx);
    }

    /// 把写信框的内容存进账号的草稿文件夹，存好后关掉写信框
    fn save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        let Some(c) = self.compose.clone() else { return };
        let r = self.built(c.read(cx), cx).and_then(|(account, b)| Ok((account, self.role_folder(account, "drafts").ok_or(t.no_drafts)?, b)));
        let (account, folder, b) = match r {
            Ok(x) => x,
            Err(e) => return status(&c, None, Some(e), cx),
        };
        let (reply, rx) = oneshot::channel();
        let _ = self.cmd.unbounded_send(Cmd::Append { account, folder, raw: b.copy, flags: "(\\Draft \\Seen)", reply: Some(reply) });
        status(&c, Some(t.saving), None, cx);
        self.wait(rx, false, window, cx);
    }

    /// 等发信（`sent`）或存草稿的结果：成功就收尾关掉写信框，失败在写信框里报原因，可以再试
    fn wait(&self, rx: oneshot::Receiver<Result<(), String>>, sent: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let r = rx.await.unwrap_or_else(|_| Err("canceled".into()));
            let _ = this.update_in(cx, |v, window, cx| v.done(r, sent, window, cx));
        })
        .detach();
    }

    fn done(&mut self, r: Result<(), String>, sent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        let Some(c) = self.compose.clone() else { return };
        if let Err(e) = r {
            return status(&c, None, Some(format!("{}{e}", if sent { t.send_failed } else { t.draft_failed })), cx);
        }
        if sent {
            if let Some((account, folder, uid)) = c.read(cx).answers
                && let Some(f) = self.folder(account, folder)
                && self.db.mark(folder, &[uid], store::ANSWERED, true).is_ok()
            {
                let _ = self.cmd.unbounded_send(Cmd::Flush { account, folder: f });
            }
            window.push_notification(Notification::success(t.sent), cx);
        }
        self.finish_compose(window, cx);
    }

    /// 关掉写信框，删掉接着写的旧草稿（发出、存了新的或丢弃后都用不着了）
    fn finish_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.compose.take() else { return };
        window.close_dialog(cx);
        if let Some((account, folder, uid)) = c.read(cx).draft {
            // Gmail 从草稿彻底删除后信还留在所有邮件里，移到已删除
            let to = self.role_folder(account, "all").and_then(|_| self.role_folder(account, "trash"));
            self.move_uids((account, folder), vec![uid], to, window, cx);
        }
    }
}

/// 改写信框底部的提示：进行中的事，或出错的原因
fn status(c: &Entity<Compose>, busy: Option<&'static str>, error: Option<String>, cx: &mut App) {
    c.update(cx, |c, cx| {
        (c.busy, c.error) = (busy, error);
        cx.notify();
    });
}

/// 发件人一栏显示的“名字 <地址>”，没有名字只显示地址
fn sender(a: &Account) -> String {
    if a.name.is_empty() { a.email.clone() } else { format!("{} <{}>", a.name, a.email) }
}

fn compose_dialog(c: &Entity<Compose>, view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let d = c.read(cx);
    let label = |s: &'static str| div().w(px(64.)).flex_none().text_sm().text_color(theme.muted_foreground).child(s);
    let me = d.senders.iter().find(|s| s.0 == d.account).map_or(String::new(), |s| s.1.clone());
    // 多个账号时可以换发件账号
    let from = if d.senders.len() > 1 {
        let (senders, current, c) = (d.senders.clone(), d.account, c.clone());
        Button::new("from")
            .ghost()
            .small()
            .label(me)
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                senders.iter().fold(menu, |menu, (id, label)| {
                    let (id, c) = (*id, c.clone());
                    menu.item(PopupMenuItem::new(label.clone()).checked(id == current).on_click(move |_, _, cx| {
                        c.update(cx, |c, cx| {
                            c.account = id;
                            cx.notify();
                        })
                    }))
                })
            })
            .into_any_element()
    } else {
        div().text_sm().child(me).into_any_element()
    };
    let more = {
        let c = c.clone();
        Button::new("more").ghost().small().label(t.cc_bcc).on_click(move |_, _, cx| {
            c.update(cx, |c, cx| {
                c.more = true;
                cx.notify();
            })
        })
    };
    let busy = d.busy.is_some();
    let failed = !busy && d.error.is_some();
    let note = d.busy.map(str::to_string).or(d.error.clone());
    let (discard, send, cancel) = (view.clone(), view.clone(), view.clone());
    dialog
        .title(title(d.title))
        .w(px(720.))
        .overlay_closable(false)
        .on_cancel(move |_, window, cx| cancel.update(cx, |v, cx| v.close_compose(window, cx)).unwrap_or(true))
        .child(
            v_flex()
                .gap_2()
                .child(h_flex().gap_2().child(label(t.from)).child(from))
                .child(h_flex().gap_2().child(label(t.to)).child(Input::new(&d.to).flex_1()).when(!d.more, |r| r.child(more)))
                .when(d.more, |v| {
                    v.child(h_flex().gap_2().child(label(t.cc)).child(Input::new(&d.cc).flex_1()))
                        .child(h_flex().gap_2().child(label(t.bcc)).child(Input::new(&d.bcc).flex_1()))
                })
                .child(h_flex().gap_2().child(label(t.subject)).child(Input::new(&d.subject).flex_1()))
                .child(Textarea::new(&d.body).h(px(320.)).font_family(theme::PROSE)),
        )
        .footer(
            h_flex()
                .gap_2()
                .child(div().flex_1().min_w_0().line_clamp(3).text_xs().text_color(if failed { theme.danger } else { theme.muted_foreground }).children(note))
                .child(Button::new("discard").label(t.discard).disabled(busy).on_click(move |_, window, cx| {
                    let _ = discard.update(cx, |v, cx| v.finish_compose(window, cx));
                }))
                .child(Button::new("send").primary().icon(IconName::Send).label(t.send).disabled(busy).on_click(move |_, window, cx| {
                    let _ = send.update(cx, |v, cx| v.send_mail(window, cx));
                })),
        )
}
