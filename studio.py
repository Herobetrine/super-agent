"""Conversation-first native workspace for the local agent scheduler."""
import json
import threading
import time
from datetime import datetime
from pathlib import Path

from PyQt6.QtCore import QSettings, QSize, Qt, QTimer, pyqtSignal
from PyQt6.QtGui import QFontMetrics, QIcon, QKeySequence, QShortcut
from PyQt6.QtSvgWidgets import QSvgWidget
from PyQt6.QtWidgets import (
    QApplication, QCheckBox, QComboBox, QDialog, QDialogButtonBox, QFileDialog,
    QFormLayout, QFrame, QHBoxLayout, QLineEdit, QListWidget, QListWidgetItem,
    QMainWindow, QMenu, QMessageBox, QPlainTextEdit, QPushButton, QScrollArea,
    QSizePolicy, QStackedWidget, QVBoxLayout, QWidget,
)

from engine import AGENTS
from permissions import PROFILES, UNRESTRICTED, DISCLAIMER
from agent_settings import AgentSettings
from studio_widgets import ASSETS, ConversationRow, Editor, MessageText, button, icon, label, public_text

TITLE = "Super Agent · 项目调度台"
NAMES = {"deepseek":"DeepSeek","codex":"Codex","claude":"Claude Code","antigravity":"Antigravity"}
STATE = {"queued":"等待开始","running":"工作中","waiting":"等待协作","draining":"等待子任务结束",
         "done":"已完成","partial":"部分完成","failed":"执行失败","interrupted":"已中断","cancelled":"已停止"}
INBOX = {"queued","running","waiting"}
ID = Qt.ItemDataRole.UserRole
LIVE_EVENT_WINDOW = 2000
LIVE_CARD_LIMIT = 100


def box(parent=None, *, vertical=True, margins=(0,0,0,0), gap=0):
    layout = QVBoxLayout(parent) if vertical else QHBoxLayout(parent)
    layout.setContentsMargins(*margins)
    layout.setSpacing(gap)
    return layout


def divider(name="divider"):
    line = QFrame()
    line.setObjectName(name)
    return line


def compact(text, n=62):
    value = " ".join(text.split())
    return value if len(value)<=n else value[:n-1]+"…"


def mark(width):
    obj = QSvgWidget(str(ASSETS/"mark.svg"))
    obj.setFixedSize(width,width)
    return obj


class Studio(QMainWindow):
    diagnostics_ready = pyqtSignal(str)

    def __init__(self,store,engine,*,settings=None):
        super().__init__()
        self.store,self.engine = store,engine
        self.settings = settings if settings is not None else QSettings("SuperAgent","Studio")
        self.tid = self.to_job = None
        self.auto_stopped = False
        self.continuous = False
        self.archive_link = None
        self.project = self.settings.value("project","",type=str)
        if self.project and not Path(self.project).is_dir():
            self.project = ""
        self.lead,self.profile = "deepseek","readonly"
        self.allow_lead_routing = True
        self.default_routes=self.store.get_routes()
        self.draft_routes=None
        self.drafts,self.tasks,self.pieces = {},[],{}
        self.cursor,self.live_widget,self.live_turn = 0,None,None
        self.history_key,self.jobs_key = None,None
        self.setWindowTitle(TITLE)
        self.setWindowIcon(QIcon(str(ASSETS/"mark.svg")))
        self.resize(1200,820)
        self.setMinimumSize(800,570)
        self.setStyleSheet((ASSETS/"studio.qss").read_text(encoding="utf-8"))
        shell = QWidget()
        self.setCentralWidget(shell)
        row = box(shell,vertical=False)
        row.addWidget(self._rail())
        self.canvas = QWidget(objectName="canvas")
        row.addWidget(self.canvas,1)
        canvas = box(self.canvas)
        canvas.addWidget(self._header())
        canvas.addWidget(divider())
        self.auto_panel = self._auto_banner()
        canvas.addWidget(self.auto_panel)
        self.auto_panel.hide()
        mainrow = box(vertical=False)
        canvas.addLayout(mainrow,1)
        column = QWidget(objectName="sheet")
        center = box(column,margins=(24,0,24,12))
        mainrow.addWidget(column,1)
        self.pages = QStackedWidget()
        center.addWidget(self.pages,1)
        self.pages.addWidget(self._welcome())
        self.scroll = QScrollArea()
        self.scroll.setWidgetResizable(True)
        self.feed = QWidget(objectName="sheet")
        self.timeline = box(self.feed,margins=(8,20,8,12),gap=12)
        self.timeline.setAlignment(Qt.AlignmentFlag.AlignTop)
        self.scroll.setWidget(self.feed)
        self.pages.addWidget(self.scroll)
        center.addWidget(self._composer())
        self.drawer = self._drawer()
        mainrow.addWidget(self.drawer)
        self.drawer.hide()
        self.diagnostics_ready.connect(self._diagnostics_done)
        self.shortcuts=[]
        for keys,fn in [("Ctrl+N",self.new_chat),("Ctrl+K",self.focus_search)]:
            key=QShortcut(QKeySequence(keys),self)
            key.activated.connect(fn)
            self.shortcuts.append(key)
        self.timer=QTimer(self)
        self.timer.setInterval(320)
        self.timer.timeout.connect(self.refresh)
        self.new_chat()
        self.timer.start()

    def _rail(self):
        self.rail = QWidget(objectName="rail")
        self.rail.setFixedWidth(230)
        layout=box(self.rail,margins=(17,23,17,14),gap=11)
        brand=box(vertical=False,gap=9)
        brand.addWidget(mark(36))
        brand.addWidget(label("Super Agent","brand"))
        brand.addStretch()
        layout.addLayout(brand)
        layout.addSpacing(12)
        layout.addWidget(button("新对话",self.new_chat,style="new",glyph="plus",tip="新对话  Ctrl+N"))
        self.search=QLineEdit(objectName="search")
        self.search.setPlaceholderText("搜索对话")
        self.search.setClearButtonEnabled(True)
        self.search.addAction(icon("search","#b4c7b8"),QLineEdit.ActionPosition.LeadingPosition)
        self.search.textChanged.connect(self._history)
        layout.addWidget(self.search)
        layout.addSpacing(7)
        layout.addWidget(label("最近对话","railCaption"))
        self.conversations=QListWidget(objectName="history")
        self.conversations.setItemDelegate(ConversationRow(self.conversations))
        self.conversations.setSpacing(2)
        self.conversations.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.conversations.itemClicked.connect(lambda item:self.open_chat(item.data(ID)))
        self.conversations.itemActivated.connect(lambda item:self.open_chat(item.data(ID)))
        layout.addWidget(self.conversations,1)
        layout.addWidget(divider("railDivider"))
        layout.addWidget(button("模型与执行端",self.configure_defaults,style="railAction",glyph="sliders",color="#bfcebf",
                                tip="选择各 agent 的供应商、模型和执行端，查看官方下载"))
        layout.addWidget(label("●   本机工作空间","railMeta"))
        return self.rail

    def _header(self):
        bar=QWidget()
        bar.setFixedHeight(64)
        row=box(bar,vertical=False,margins=(15,0,16,0),gap=5)
        row.addWidget(button("",self.toggle_rail,glyph="menu",tip="展开或收起会话列表"))
        self.heading=label("新对话","title")
        self.heading.setMinimumWidth(30)
        self.heading.setSizePolicy(QSizePolicy.Policy.Ignored,QSizePolicy.Policy.Preferred)
        row.addWidget(self.heading,1)
        self.approval_button=button("待审批",self.approvals,style="risk",tip="查看并处理所有工单待批准的动作")
        row.addWidget(self.approval_button)
        self.badge=label("待开始","pill")
        row.addWidget(self.badge,0,Qt.AlignmentFlag.AlignVCenter)
        self.detail_button=button("协作",self.toggle_drawer,glyph="team",tip="查看已启用的 agent 和它们的任务")
        row.addWidget(self.detail_button)
        more=button("",self.more,glyph="dots",tip="更多操作")
        row.addWidget(more)
        self.more_button=more
        return bar

    def _auto_banner(self):
        panel=QFrame(objectName="activity")
        layout=box(panel,margins=(16,5,16,6),gap=2)
        top=box(vertical=False,gap=8)
        self.auto_phase_label=label("", "speaker")
        top.addWidget(self.auto_phase_label)
        top.addStretch()
        self.auto_stop_button=button("停止自动运行",self.cancel,style="risk",glyph="stop",
                                     tip="停止这张工单的运行和后续自动调度")
        top.addWidget(self.auto_stop_button)
        layout.addLayout(top)
        self.auto_goal_label=label("", "hint")
        self.auto_memory_label=label("", "micro")
        for item in (self.auto_goal_label,self.auto_memory_label):
            item.setSizePolicy(QSizePolicy.Policy.Ignored,QSizePolicy.Policy.Preferred)
            layout.addWidget(item)
        return panel

    def _welcome(self):
        page=QWidget(objectName="hero")
        col=box(page,margins=(0,6,0,12),gap=4)
        col.addStretch(2)
        illustration=QSvgWidget(str(ASSETS/"orbit.svg"))
        illustration.setFixedSize(292,117)
        col.addWidget(illustration,0,Qt.AlignmentFlag.AlignHCenter)
        for text,style in [("SUPER AGENT   /   WORKSPACE","eyebrow"),("今天想完成什么？","heroTitle"),
                           ("告诉我目标，我会决定直接处理，或邀请合适的 agent 一起工作。","hint")]:
            item=label(text,style,wrap=True)
            item.setAlignment(Qt.AlignmentFlag.AlignCenter)
            col.addWidget(item)
        col.addSpacing(19)
        cards=box(vertical=False,gap=9)
        entries=[("map","梳理项目","了解目录与关键模块","请梳理这个项目的目录、关键模块和运行方式，给我一份可以直接上手的说明。"),
                 ("code","定位问题","从复现到验证","请帮我定位并修复这个问题，验证修复结果。\n\n复现步骤："),
                 ("book","讨论方案","比较取舍与成本","请和我讨论下面的需求，比较可行方案、实施成本与取舍。\n\n需求：")]
        for glyph,title,sub,prompt in entries:
            card=button("",lambda _,p=prompt:self.suggest(p),style="suggest",tip=title)
            card.setMinimumHeight(84)
            card.setSizePolicy(QSizePolicy.Policy.Expanding,QSizePolicy.Policy.Fixed)
            inner=box(card,margins=(14,8,8,9),gap=2)
            top=box(vertical=False,gap=7)
            glyph_label=label()
            glyph_label.setPixmap(icon(glyph).pixmap(18,18))
            top.addWidget(glyph_label)
            top.addWidget(label(title,"speaker"))
            top.addStretch()
            inner.addLayout(top)
            inner.addWidget(label(sub,"micro"))
            cards.addWidget(card)
        col.addLayout(cards)
        col.addStretch(3)
        return page

    def _composer(self):
        outer=QWidget()
        col=box(outer,gap=5)
        self.notice=label("","notice",wrap=True)
        self.notice.hide()
        col.addWidget(self.notice)
        frame=QFrame(objectName="composer")
        inner=box(frame,margins=(14,9,12,9),gap=5)
        self.recipient=button("",self.clear_target,style="chip",glyph="close",tip="取消定向投递")
        inner.addWidget(self.recipient,0,Qt.AlignmentFlag.AlignLeft)
        self.editor=Editor(objectName="editor")
        self.editor.setFixedHeight(66)
        self.editor.setTabChangesFocus(True)
        self.editor.submit.connect(self.send)
        self.editor.textChanged.connect(self._send_state)
        inner.addWidget(self.editor)
        permission_row=box(vertical=False,gap=8)
        self.permission_label=label("权限","permissionLabel")
        self.permission_picker=QComboBox(objectName="permissionProfile")
        self.permission_picker.setAccessibleName("本次会话权限")
        self.permission_label.setBuddy(self.permission_picker)
        for key,(name,description) in PROFILES.items():
            display=name+" · 高危" if key in UNRESTRICTED and "高危" not in name else name
            self.permission_picker.addItem(display,key)
            self.permission_picker.setItemData(self.permission_picker.count()-1,description,Qt.ItemDataRole.ToolTipRole)
        self.permission_picker.activated.connect(self._pick_permission)
        self.permission_hint=label("","micro",wrap=True)
        permission_row.addWidget(self.permission_label)
        permission_row.addWidget(self.permission_picker)
        permission_row.addWidget(self.permission_hint,1)
        inner.addLayout(permission_row)
        actions=box(vertical=False,gap=4)
        self.project_button=button("选择项目",self.browse,style="chip",glyph="folder")
        self.mode_button=button("",self.options,style="chip",glyph="sliders",tip="会话设置：主控、模型与协作者授权")
        actions.addWidget(self.project_button)
        actions.addWidget(self.mode_button)
        actions.addStretch()
        self.send_button=button("发送",self.send,style="send",glyph="arrow",color="#ffffff",tip="发送消息  Ctrl+Enter")
        actions.addWidget(self.send_button)
        inner.addLayout(actions)
        col.addWidget(frame)
        foot=box(vertical=False,gap=4)
        self.route_hint=label("","micro")
        foot.addWidget(self.route_hint,1)
        foot.addWidget(label("Ctrl + Enter 发送   ·   Enter 换行","micro"))
        col.addLayout(foot)
        return outer

    def _drawer(self):
        frame=QFrame(objectName="drawer")
        frame.setFixedWidth(274)
        col=box(frame,margins=(14,14,14,14),gap=8)
        head=box(vertical=False)
        head.addWidget(label("本次协作","title"),1)
        head.addWidget(button("",self.toggle_drawer,glyph="close",tip="关闭协作面板"))
        col.addLayout(head)
        self.jobs_hint=label("任务出现后，可在这里查看分工和回报。","micro",wrap=True)
        col.addWidget(self.jobs_hint)
        self.jobs_list=QListWidget(objectName="jobs")
        self.jobs_list.currentItemChanged.connect(self._job_detail)
        col.addWidget(self.jobs_list,1)
        self.job_text=QPlainTextEdit()
        self.job_text.setReadOnly(True)
        self.job_text.setPlaceholderText("选中任务查看详情")
        col.addWidget(self.job_text,2)
        self.note_button=button("给这个任务留言",self.note_selected,style="chip",glyph="chat")
        self.retry_button=button("重试此任务",self.retry,style="chip")
        col.addWidget(self.note_button)
        col.addWidget(self.retry_button)
        return frame

    def toggle_rail(self):
        if self.rail.isHidden() and self.width()<1100:
            self.drawer.hide()
        self.rail.setVisible(self.rail.isHidden())

    def resizeEvent(self,event):
        super().resizeEvent(event)
        if hasattr(self,"drawer") and self.width()<1100 and not self.drawer.isHidden():
            self.rail.hide()

    def focus_search(self):
        self.rail.show()
        if self.width()<1100:
            self.drawer.hide()
        self.search.setFocus()

    def toggle_drawer(self):
        show=self.drawer.isHidden()
        self.drawer.setVisible(show)
        if show and self.width()<1100:
            self.rail.hide()

    def suggest(self,text):
        self.editor.setPlainText(text)
        self.editor.moveCursor(self.editor.textCursor().MoveOperation.End)
        self.editor.setFocus()

    def _save_draft(self):
        self.drafts[(self.tid,self.to_job)]=self.editor.toPlainText()

    def _restore_draft(self):
        self.editor.setPlainText(self.drafts.get((self.tid,self.to_job),""))

    def _clear_feed(self):
        while self.timeline.count():
            item=self.timeline.takeAt(0)
            if item.widget():
                item.widget().deleteLater()
        self.cursor=0
        self.live_widget=self.live_turn=None
        self.archive_link=None
        self.pieces.clear()
        self.jobs_key=None

    def new_chat(self):
        if self.tid is not None or self.to_job is not None:
            self._save_draft()
            self.draft_routes=None
        elif self.editor.toPlainText():
            self._save_draft()
        self.profile="readonly"
        self.allow_lead_routing=True
        self.tid=self.to_job=None
        self.info=None
        self.tasks=[]
        self.conversations.clearSelection()
        self._clear_feed()
        self.notice.hide()
        self.drawer.hide()
        self.pages.setCurrentIndex(0)
        self._restore_draft()
        self.refresh()
        self.editor.setFocus()

    def open_chat(self,tid):
        if tid==self.tid:
            return
        rows=self.store.rows("SELECT * FROM tickets WHERE id=?",(tid,))
        if not rows:
            return
        self._save_draft()
        self.tid,self.info,self.to_job=tid,rows[0],None
        self._clear_feed()
        self.notice.hide()
        self.pages.setCurrentIndex(1)
        self._restore_draft()
        self.refresh()
        self.scroll.verticalScrollBar().setValue(self.scroll.verticalScrollBar().maximum())

    def set_project(self,path):
        path=str(Path(path).resolve(strict=True))
        if self.tid is not None:
            raise ValueError("新对话可以选择其他项目")
        if not Path(path).is_dir():
            raise ValueError("请选择目录")
        if path!=self.project:
            self.profile="readonly"
        self.project=path
        self.settings.setValue("project",path)
        self._controls()

    def browse(self):
        if self.tid is not None:
            self.options()
            return
        value=QFileDialog.getExistingDirectory(self,"选择项目目录",self.project or str(Path.home()))
        if value:
            self.set_project(value)

    def configure_defaults(self):
        self.configure_agents(defaults=True)

    def configure_agents(self,*,defaults=False):
        tid=None if defaults else self.tid
        routes=self.store.get_routes(tid)
        if not defaults and tid is None and self.draft_routes is not None:
            routes=self.draft_routes
        locked=bool(tid and any(j["state"] not in ("done","failed","partial","cancelled") for j in self.tasks))
        dialog=AgentSettings(self.engine.adapters,routes,self,ticket=tid,locked=locked,
                             initial=self.info["lead"] if tid else self.lead,
                             legacy_route=self.info["claude_route"] if tid else None,
                             draft=not defaults and tid is None)
        if dialog.exec()!=QDialog.DialogCode.Accepted or locked:
            return
        chosen=dialog.selections()
        try:
            if defaults or tid:
                self.store.save_routes(chosen,ticket=tid)
            if defaults:
                self.default_routes=chosen
            elif tid:
                self.info["agent_routes"]=json.dumps(chosen)
            else:
                self.draft_routes=chosen
            self._controls()
        except (ValueError,OSError) as exc:
            self.message(str(exc))

    def options(self):
        dlg=QDialog(self)
        dlg.setWindowTitle("会话设置")
        dlg.setMinimumWidth(380)
        col=box(dlg,margins=(22,18,22,16),gap=13)
        form=QFormLayout()
        current=self.info if self.tid is not None else {}
        lead=QComboBox()
        for key in AGENTS:
            lead.addItem(NAMES[key],key)
        lead.setCurrentIndex(lead.findData(current.get("lead",self.lead)))
        form.addRow("主控模型入口",lead)
        form.addRow("各 agent 的执行配置",button("选择供应商、模型和执行端…",lambda:self.configure_agents(),style="chip"))
        delegated=QCheckBox("允许主控为协作者选择模型和子任务权限")
        delegated.setObjectName("allowLeadRouting")
        delegated.setChecked(bool(current.get("allow_lead_routing",self.allow_lead_routing)))
        form.addRow("协作者授权",delegated)
        form.addRow("本次权限",label(self.permission_picker.currentText()))
        form.addRow("工作目录",label(current.get("cwd",self.project) or "独立临时项目",wrap=True))
        col.addLayout(form)
        fixed=self.tid is not None
        for w in (lead,delegated):
            w.setEnabled(not fixed)
        col.addWidget(label("协作者权限以本次会话的授权为上限。权限预设位于输入框下方，发送首条消息后锁定。","micro",wrap=True))
        buttons=QDialogButtonBox(QDialogButtonBox.StandardButton.Ok if fixed else
                                 QDialogButtonBox.StandardButton.Ok|QDialogButtonBox.StandardButton.Cancel)
        buttons.accepted.connect(dlg.accept)
        buttons.rejected.connect(dlg.reject)
        col.addWidget(buttons)
        if dlg.exec() and not fixed:
            self.lead,self.allow_lead_routing=lead.currentData(),delegated.isChecked()
            self._controls()

    def _pick_permission(self,index):
        key=self.permission_picker.itemData(index)
        if self.tid is not None or key not in PROFILES or key==self.profile:
            return
        if key in UNRESTRICTED:
            # Keep the actual selection unchanged until this specific grant is confirmed.
            self.permission_picker.setCurrentIndex(self.permission_picker.findData(self.profile))
            dlg=QMessageBox(self)
            dlg.setWindowTitle("确认本次高危授权")
            dlg.setIcon(QMessageBox.Icon.Warning)
            dlg.setText("为本次新会话启用“"+self.permission_picker.itemText(index)+"”？")
            dlg.setInformativeText(PROFILES[key][1]+"\n\n"+DISCLAIMER)
            cancel=dlg.addButton(QMessageBox.StandardButton.Cancel)
            grant=dlg.addButton("我已了解风险，启用此档位",QMessageBox.ButtonRole.AcceptRole)
            dlg.setDefaultButton(cancel)
            dlg.setEscapeButton(cancel)
            dlg.exec()
            if dlg.clickedButton()!=grant:
                return
        self.profile=key
        self._controls()

    def _controls(self):
        info=self.info or {}
        path=info.get("cwd",self.project)
        lead=info.get("lead",self.lead)
        profile=info.get("permission_profile",self.profile)
        self.project_button.setText(compact(Path(path).name,18) if path else "选择项目")
        self.project_button.setToolTip(path or "可选；直接发送将在独立临时目录中工作")
        self.mode_button.setText("主控 · "+NAMES[lead])
        legacy=self.permission_picker.findData("legacy")
        if profile=="legacy":
            legacy_text="旧工单 · "+("受控写入" if info.get("writes") else "只读")
            if legacy<0:
                self.permission_picker.addItem(legacy_text,"legacy")
            else:
                self.permission_picker.setItemText(legacy,legacy_text)
        elif legacy>=0:
            self.permission_picker.removeItem(legacy)
        self.permission_picker.setCurrentIndex(self.permission_picker.findData(profile))
        is_draft=self.tid is None
        self.permission_picker.setEnabled(is_draft)
        self.permission_picker.setToolTip(PROFILES[profile][1] if profile in PROFILES else "沿用创建时的权限")
        self.permission_hint.setText("发送首条消息前可选择" if is_draft else "本工单已锁定 · 新建对话可更改")
        risky=profile in UNRESTRICTED
        if self.permission_picker.property("highRisk")!=risky:
            self.permission_picker.setProperty("highRisk",risky)
            self.style().unpolish(self.permission_picker)
            self.style().polish(self.permission_picker)
        selections=json.loads(info["agent_routes"]) if info else (
            self.default_routes if self.draft_routes is None else self.draft_routes)
        picked=selections.get(lead)
        self.route_hint.setText((("CLI" if picked["mode"]=="cli" else "桌面端")+"  /  "+picked["model"])
                                if picked else "跟随本机适配器默认配置")
        self.recipient.setVisible(self.to_job is not None)
        self.editor.setPlaceholderText("给任务留言…" if self.to_job else
                                       ("输入恢复后要做的工作…" if self.auto_stopped else
                                        "继续这次对话，或补充新的要求…" if self.tid else "描述你的目标，或先选一个项目…"))
        self.send_button.setText("恢复并发送" if self.auto_stopped else "发送")
        self.send_button.setToolTip("确认后以这条新指令恢复自动运行" if self.auto_stopped else "发送消息  Ctrl+Enter")
        self.detail_button.setEnabled(bool(self.tid))
        self._send_state()

    def _send_state(self):
        if hasattr(self,"send_button"):
            self.send_button.setEnabled(bool(self.editor.toPlainText().strip()))

    def send(self):
        body=self.editor.toPlainText()
        if not body.strip():
            return
        try:
            if (self.tid and self.info["permission_profile"]=="auto" and self.to_job is None
                    and body.strip().rstrip("。！!") in ("停止","停止工单","停止自动运行","/stop")):
                self.cancel()
                self.editor.clear()
                return
            if self.auto_stopped:
                confirm=QMessageBox.question(self,"恢复自动运行","以输入框中的这条新指令恢复完全访问和持续模型调用？",
                      QMessageBox.StandardButton.Yes|QMessageBox.StandardButton.No,QMessageBox.StandardButton.No)
                if confirm!=QMessageBox.StandardButton.Yes:
                    return
                self.store.follow(self.tid,body,resume=True)
                self.editor.clear()
            elif self.tid is None:
                if self.project:
                    cwd=Path(self.project)
                else:
                    import tempfile
                    spaces=self.store.path/"workspaces"
                    spaces.mkdir(parents=True,exist_ok=True)
                    cwd=Path(tempfile.mkdtemp(prefix="chat-",dir=spaces))
                tid=self.store.create(body,cwd,self.lead,selections=self.draft_routes,permission_profile=self.profile,
                                      allow_lead_routing=self.allow_lead_routing)
                self.editor.clear()
                self.drafts.pop((None,None),None)
                self.open_chat(tid)
            elif self.to_job is not None:
                self.store.note_user(self.tid,self.to_job,body)
                self.editor.clear()
                self.clear_target()
            else:
                self.store.follow(self.tid,body)
                self.editor.clear()
            self.notice.hide()
            self.refresh()
        except (ValueError,OSError,RuntimeError) as exc:
            self.message(str(exc))

    def message(self,text):
        self.notice.setText(text)
        self.notice.show()

    def target_job(self,jid):
        target=next((j for j in self.tasks if j["id"]==jid and j["state"] in INBOX),None)
        if not target:
            self.message("该任务已结束或不属于当前对话")
            return
        self._save_draft()
        self.to_job=jid
        self._restore_draft()
        self.recipient.setText(f'仅投递给 {NAMES[target["agent"]]}  ·  #{jid}')
        self._controls()
        self.editor.setFocus()

    def clear_target(self):
        self._save_draft()
        self.to_job=None
        self._restore_draft()
        self._controls()

    def note_selected(self):
        selected=self.jobs_list.currentItem()
        if selected:
            self.target_job(selected.data(ID))

    def _history(self,*_):
        term=self.search.text().strip()
        rows=self.store.rows("""SELECT t.id,substr(t.goal,1,120) AS title,t.cwd,j.state
              FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL
              WHERE instr(lower(t.goal),lower(?))>0 OR instr(lower(t.cwd),lower(?))>0
              ORDER BY t.id DESC LIMIT 160""",(term,term))
        key=(rows,self.tid)
        if key==self.history_key:
            return
        self.history_key=key
        self.conversations.clear()
        for r in rows:
            text=compact(r["title"],60)
            item=QListWidgetItem(text)
            item.setData(ID,r["id"])
            item.setData(ID+1,f'#{r["id"]}  ·  {STATE[r["state"]]}  ·  {Path(r["cwd"]).name}')
            item.setToolTip(f'{text}\n{r["cwd"]}\n{STATE[r["state"]]}')
            self.conversations.addItem(item)
            if r["id"]==self.tid:
                self.conversations.setCurrentItem(item)

    def _job_detail(self,*_):
        item=self.jobs_list.currentItem()
        job=next((j for j in self.tasks if item and j["id"]==item.data(ID)),None)
        self.note_button.setEnabled(bool(job and job["state"] in INBOX))
        self.retry_button.setEnabled(bool(job and not self.auto_stopped and job["state"] in ("failed","interrupted")))
        bound_route=job["route"] if job else None
        if job:
            job=self.store.rows("SELECT * FROM jobs WHERE id=?",(job["id"],))[0]
        dest=None
        source="工单配置"
        if job:
            dest=json.loads(self.info["agent_routes"]).get(job["agent"])
            if job.get("route_override"):
                dest=json.loads(job["route_override"])
                source="主控指定"
            if bound_route:
                dest=json.loads(bound_route)
                source="执行时绑定"
        path=(f'{source}\n{dest["provider"]} / {dest["model"]} · {dest["mode"]}' if dest else "跟随适配器默认")
        profile=(job.get("permission_override") or self.info["permission_profile"]) if job else None
        permission=PROFILES[profile][0] if profile in PROFILES else ("受控写入" if job and job["writing"] else "只读")
        if job:
            permission+=" · "+("主控指定" if job.get("permission_override") else "沿用工单")
        self.job_text.setPlainText(("" if not job else
            "任务内容\n"+job["body"]+"\n\n当前职能\n"+(job.get("role") or "尚未命名")+
            "\n\n任务执行配置\n"+path+
            "\n\n任务权限\n"+permission+
            ("\n\n最近回报\n"+job["result"] if job.get("result") else "")))

    def _jobs(self):
        where="j.ticket=?"
        params=(self.tid,)
        if self.continuous:
            where="""j.id IN (SELECT id FROM jobs WHERE ticket=? AND parent IS NULL
                      UNION SELECT id FROM jobs WHERE ticket=? AND state IN ('running','queued','waiting','draining')
                      UNION SELECT id FROM (SELECT id FROM jobs WHERE ticket=? ORDER BY id DESC LIMIT 40))"""
            params=(self.tid,)*3
        rows=self.store.rows("""SELECT j.id,j.parent,j.agent,j.state,j.turn,j.writing,j.role,
                                      substr(j.body,1,80) AS body,j.permission_override,j.route_override,n.route
                                FROM jobs j LEFT JOIN native_routes n ON n.job=j.id
                                WHERE """+where+" ORDER BY j.id",params) if self.tid else []
        if rows!=self.jobs_key:
            self.tasks=self.jobs_key=rows
            selected=self.jobs_list.currentItem()
            jid=selected.data(ID) if selected else None
            self.jobs_list.blockSignals(True)
            self.jobs_list.clear()
            for j in rows:
                sub=j.get("role") or ("主控" if j["parent"] is None else compact(j["body"],17))
                item=QListWidgetItem(f'{NAMES[j["agent"]]}   ·   {STATE[j["state"]]}\n{sub}')
                item.setData(ID,j["id"])
                item.setToolTip(j["body"])
                self.jobs_list.addItem(item)
                if j["id"]==jid:
                    self.jobs_list.setCurrentItem(item)
            if rows and not self.jobs_list.currentItem():
                self.jobs_list.setCurrentRow(0)
            self.jobs_list.blockSignals(False)
            self._job_detail()
        self.jobs_hint.setText("显示主控、活动任务及最近 40 项。完整派工可导出。" if self.continuous else
                               "任务出现后，可在这里查看分工和回报。")
        self.detail_button.setText(f"协作  {len(rows)}" if rows and not self.continuous else "协作")
        self.badge.setText(STATE[rows[0]["state"]] if rows else "待开始")

    def _message(self,name,body,stamp,*,user=False,live=False):
        frame=QFrame(objectName="userCard" if user else "assistantCard")
        col=box(frame,margins=(14,10,14,11) if user else (0,7,0,5),gap=6)
        head=box(vertical=False,gap=8)
        if not user:
            head.addWidget(mark(23))
        head.addWidget(label(name,"speaker"))
        head.addWidget(label(stamp,"micro"))
        head.addStretch()
        copy=button("",lambda:QApplication.clipboard().setText(frame.original),glyph="copy",tip="复制消息原文")
        copy.setFixedSize(28,25)
        head.addWidget(copy)
        col.addLayout(head)
        view=MessageText()
        col.addWidget(view)
        frame.view,frame.original=view,body
        view.put(body[:32000]+("\n…（可复制完整消息）" if len(body)>32000 else ""),not user and not live)
        self.timeline.addWidget(frame)
        return frame

    def _activity(self,msg,error=False):
        if self.continuous and len(msg)>2400:
            msg=msg[:2400]+"…（可导出完整记录）"
        frame=QFrame(objectName="errorCard" if error else "activity")
        row=box(frame,vertical=False,margins=(10,6,9,6),gap=8)
        sig=label()
        sig.setPixmap(icon("activity").pixmap(16,16))
        row.addWidget(sig)
        row.addWidget(label(msg,"hint",wrap=True),1)
        self.timeline.addWidget(frame)

    def _remove_live(self):
        if self.live_widget:
            self.timeline.removeWidget(self.live_widget)
            self.live_widget.deleteLater()
            self.live_widget=None
        self.live_turn=None

    def _auto_progress(self):
        if not self.info or self.info["permission_profile"]!="auto":
            self.auto_stopped=False
            self.continuous=False
            self.auto_panel.hide()
            return
        data=self.store.rows("""SELECT t.objective,c.phase,c.milestone,c.learning,j.state,j.ready_after
                    FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL
                    LEFT JOIN continuous_runs c ON c.ticket=t.id WHERE t.id=?""",(self.tid,))[0]
        state=data["state"]
        phase=data["phase"]
        self.continuous=phase is not None
        self.auto_stopped=state=="cancelled"
        if self.auto_stopped and self.to_job is not None:
            self.clear_target()
        active=state in INBOX or state=="draining"
        if self.auto_stopped:
            text="已停止 · 需明确恢复才会继续"
        elif not active:
            text="已提交穷尽说明" if phase=="exhausted" else STATE.get(state,state)
        elif phase=="discover":
            text="正在搜寻下一步"
        elif phase=="work":
            text=f"执行阶段 {data['milestone']+1}"
        else:
            text="原工单 · "+STATE.get(state,state)
        remaining=int((data["ready_after"] or 0)-time.time())
        if state=="queued" and remaining>0:
            text+=f" · {remaining} 秒后再试"
        self.auto_phase_label.setText("自动运行   /   "+text)
        self.auto_stop_button.setVisible(active)
        self.auto_panel.show()
        fields=[(self.auto_goal_label,"当前目标",data["objective"] or self.info["goal"]),
                (self.auto_memory_label,"最近经验",data["learning"] or "暂无记录")]
        for widget,title,body in fields:
            full=title+" · "+" ".join(body.split())
            widget.setText(QFontMetrics(widget.font()).elidedText(full,Qt.TextElideMode.ElideRight,
                                                                  max(100,widget.width())))
            widget.setToolTip(full[:1200])
        self.auto_memory_label.setVisible(phase is not None)

    def _archive_preview(self):
        if not self.archive_link:
            self.archive_link=button("正在显示近期活动 · 导出完整记录",self.export,style="chip",glyph="book")
            self.timeline.insertWidget(0,self.archive_link,0,Qt.AlignmentFlag.AlignLeft)
        while self.timeline.count()>LIVE_CARD_LIMIT+1:
            old=self.timeline.takeAt(1).widget()
            if old is self.live_widget:
                self.live_widget=self.live_turn=None
            old.hide()
            old.deleteLater()

    def _events(self):
        if not self.tasks:
            return
        root=self.tasks[0]
        upper=2**63-1
        order="ORDER BY e.seq LIMIT 250"
        if self.continuous:
            upper=self.store.rows("SELECT max(seq) AS hi FROM events WHERE ticket=?",(self.tid,))[0]["hi"] or 0
            if upper<=self.cursor:
                return
            cutoff=self.store.rows("""SELECT seq FROM events WHERE ticket=? AND seq>? AND seq<=?
                                  ORDER BY seq DESC LIMIT 1 OFFSET ?""",(self.tid,self.cursor,upper,LIVE_EVENT_WINDOW))
            if cutoff:
                self.cursor=cutoff[0]["seq"]
                self._remove_live()
                self.pieces.clear()
                self._archive_preview()
            order=f"ORDER BY e.seq DESC LIMIT {LIVE_CARD_LIMIT+1}"
        sql="""SELECT e.seq,e.ts,e.kind,e.payload,e.job,e.turn,j.agent FROM events e JOIN jobs j ON j.id=e.job
               WHERE e.ticket=? AND e.seq>? AND e.seq<=? AND (e.kind IN ('user','user.follow','send','note','role','failed',
                'cancelled','interrupted','draining','note.rejected','auto.goal','auto.next','auto.finish','auto.recover',
                'auto.milestone','auto.learn','auto.discovered','auto.exhausted','auto.retry','auto.backoff',
                'approval.request','approval.decision') OR
                (e.job=? AND (e.kind='reply' OR (e.kind='delta' AND j.state='running' AND e.turn=j.turn))))
               """+order
        rows=self.store.rows(sql,(self.tid,self.cursor,upper,root["id"]))
        if self.continuous:
            if len(rows)>LIVE_CARD_LIMIT:
                self._remove_live()
                self.pieces.clear()
                self._archive_preview()
                rows.pop()
            rows.reverse()
            self.cursor=upper
        if not rows:
            return
        scroll=self.scroll.verticalScrollBar()
        bottom=scroll.value()>=scroll.maximum()-8
        for e in rows:
            if not self.continuous:
                self.cursor=e["seq"]
            k=e["kind"]
            p=json.loads(e["payload"])
            agent=NAMES[e["agent"]]
            stamp=datetime.fromtimestamp(e["ts"]).strftime("%H:%M")
            if k in ("user","user.follow"):
                self._message("你",p,stamp,user=True)
            elif k=="delta" and isinstance(p,str):
                self.pieces.setdefault(e["turn"],[]).append(p)
            elif k=="reply":
                self._remove_live()
                self.pieces.pop(e["turn"],None)
                body=public_text(p["text"])
                if body:
                    frame=self._message(agent,body,stamp)
                    frame.original=p["text"]
            elif k=="send":
                role=p.get("role") or compact(p["body"],60)
                self._activity(f'{agent} 邀请 {NAMES[p["to"]]} · {role}')
            elif k=="role":
                self._activity(agent+" · "+str(p.get("role","") if isinstance(p,dict) else p))
            elif k in ("note","note.rejected"):
                self._activity(f'{p["sender"]} → 任务 #{p["job"]}  ·  '+compact(p["body"],90),k=="note.rejected")
            elif k=="auto.goal":
                self._activity("当前阶段目标 · "+p["objective"])
            elif k=="auto.next":
                self._activity("自动续轮 · "+p["body"])
            elif k=="auto.milestone":
                status="遇阻" if p["status"]=="blocked" else "已完成"
                self._activity(f'阶段{status} · {p["summary"]}')
            elif k=="auto.learn":
                self._activity("经验摘要 · "+p["lesson"])
            elif k=="auto.discovered":
                self._activity(f'选定下一步 · {p["objective"]}\n依据：{p["evidence"]}\n行动：{p["body"]}')
            elif k=="auto.exhausted":
                checks="；".join(p["checks"])
                alternatives="；".join(p["alternatives"])
                self._activity(f'穷尽说明 · {p["summary"]}\n已核查：{checks}\n替代路径：{alternatives}',
                               p["status"]=="blocked")
            elif k in ("auto.retry","auto.backoff"):
                self._activity(f'自动重试 · 等待 {p["after_seconds"]:g} 秒\n{p["reason"]}')
            elif k=="auto.finish":
                self._activity(("阻塞 · " if p["status"]=="blocked" else "目标已核验 · ")+p["summary"],p["status"]=="blocked")
            elif k=="auto.recover":
                self._activity("恢复核对 · "+p)
            elif k=="approval.request":
                self._activity(f'{agent} 请求审批 #{p["approval"]}，请查看顶部待审批按钮。')
            elif k=="approval.decision":
                self._activity(f'审批 #{p["approval"]} · '+("已允许本次请求" if p["decision"]=="allow" else "已拒绝"))
            else:
                if e["job"]==root["id"]:
                    self._remove_live()
                    self.pieces.pop(e["turn"],None)
                self._activity(f'{agent} · {STATE.get(k,k)}\n{str(p)[:1200]}',True)
        if root["state"]=="running" and root["turn"] in self.pieces:
            accumulated="".join(self.pieces[root["turn"]])
            if self.continuous and len(accumulated)>32000:
                accumulated=accumulated[-32000:]
                self.pieces[root["turn"]]=[accumulated]
            text=public_text(accumulated)
            if text and not text.startswith(("SA_","SA ")):
                if self.live_turn!=root["turn"]:
                    self._remove_live()
                    self.live_widget=self._message(NAMES[root["agent"]],text,"正在回复",live=True)
                    self.live_turn=root["turn"]
                else:
                    self.live_widget.original=text
                    self.live_widget.view.put(text[-32000:],False)
        if self.continuous and self.timeline.count()>LIVE_CARD_LIMIT:
            self._archive_preview()
        if bottom:
            QTimer.singleShot(0,lambda:scroll.setValue(scroll.maximum()))

    def refresh(self):
        self._history()
        self._auto_progress()
        self._jobs()
        self._events()
        count=self.store.rows("SELECT count(*) AS n FROM approvals WHERE decision='pending'")[0]["n"]
        self.approval_button.setText(f"待审批 {count}")
        self.approval_button.setVisible(bool(count))
        full=compact(self.info["goal"]) if self.info else "新对话"
        width=max(75,self.heading.contentsRect().width())
        self.heading.setText(QFontMetrics(self.heading.font()).elidedText(full,Qt.TextElideMode.ElideRight,width))
        self.heading.setToolTip(full)
        self._controls()
        if self.engine.fault:
            self.message(self.engine.fault)

    def approvals(self):
        rows=self.store.rows("""SELECT a.*,j.agent,t.cwd FROM approvals a JOIN jobs j ON j.id=a.job
                             JOIN tickets t ON t.id=a.ticket WHERE a.decision='pending' ORDER BY a.id""")
        if not rows:
            return
        dlg=QDialog(self)
        dlg.setWindowTitle("审批本次请求")
        dlg.resize(630,430)
        col=box(dlg,margins=(16,14,16,14),gap=9)
        choices=QComboBox()
        for r in rows:
            choices.addItem(f'工单 #{r["ticket"]} · {NAMES[r["agent"]]} · 任务 #{r["job"]} · 申请 #{r["id"]}')
        details=QPlainTextEdit()
        details.setReadOnly(True)
        def show_request(index):
            r=rows[index]
            details.setPlainText("工作目录："+r["cwd"]+"\n\n"+json.dumps(json.loads(r["action"]),ensure_ascii=False,indent=2))
        choices.currentIndexChanged.connect(show_request)
        show_request(0)
        col.addWidget(choices)
        col.addWidget(details,1)
        def decide(allow):
            try:
                self.store.decide(rows[choices.currentIndex()]["id"],allow)
            except ValueError as exc:
                self.message(str(exc))
            dlg.accept()
        row=box(vertical=False,gap=8)
        row.addStretch()
        row.addWidget(button("暂不处理",dlg.reject))
        row.addWidget(button("拒绝",lambda:decide(False)))
        row.addWidget(button("允许本次申请",lambda:decide(True),style="risk"))
        col.addLayout(row)
        dlg.exec()
        self.refresh()

    def more(self):
        menu=QMenu(self)
        for title,fn,active in [("会话设置",self.options,True),("本次对话的模型",lambda:self.configure_agents(),True),
                                ("连接与诊断",self.probe,True),("查看事件记录",self.raw_events,bool(self.tid)),
                                ("导出完整记录…",self.export,bool(self.tid)),
                                ("停止执行",self.cancel,any(j["state"] in INBOX or j["state"]=="draining" for j in self.tasks))]:
            action=menu.addAction(title)
            action.setEnabled(active)
            action.triggered.connect(fn)
        menu.exec(self.more_button.mapToGlobal(self.more_button.rect().bottomLeft()))

    def raw_events(self):
        rows=self.store.rows("SELECT seq,kind,job,substr(payload,1,900) AS preview FROM events WHERE ticket=? ORDER BY seq DESC LIMIT 100",(self.tid,))
        dialog=QDialog(self)
        dialog.setWindowTitle("最近 100 条事件 · 完整正文可导出")
        dialog.resize(690,480)
        view=QPlainTextEdit()
        view.setReadOnly(True)
        view.setPlainText("\n\n".join(f'{r["seq"]}   #{r["job"]}   {r["kind"]}\n{r["preview"]}' for r in reversed(rows)))
        box(dialog,margins=(12,12,12,12)).addWidget(view)
        dialog.exec()

    def export(self):
        dest,_=QFileDialog.getSaveFileName(self,"导出完整记录",f"super-agent-{self.tid}.jsonl","JSONL (*.jsonl)")
        if dest:
            try:
                self.store.export(self.tid,dest)
                self.message("记录已校验并导出到 "+dest)
            except (OSError,ValueError) as exc:
                self.message(str(exc))

    def retry(self):
        if self.auto_stopped:
            self.message("此工单已停止。请输入新指令并明确恢复。")
            return
        selected=self.jobs_list.currentItem()
        if selected:
            try:
                self.store.retry(selected.data(ID))
                self.refresh()
            except ValueError as exc:
                self.message(str(exc))

    def cancel(self):
        if self.tid:
            self.engine.cancel(self.tid)
            self.refresh()

    def probe(self):
        if getattr(self,"probing",False):
            return
        self.probing=True
        self.message("正在检查本机入口…")
        def check():
            lines=[]
            for key,adapter in self.engine.adapters.items():
                try:
                    lines.append(NAMES[key]+"\n"+str(adapter.probe()["detail"]))
                except Exception as exc:
                    lines.append(NAMES[key]+"\n"+str(exc))
            self.diagnostics_ready.emit("\n\n".join(lines))
        threading.Thread(target=check,daemon=True).start()

    def _diagnostics_done(self,text):
        self.probing=False
        self.notice.hide()
        QMessageBox.information(self,"连接与诊断 · 入口检测",text)

    def closeEvent(self,event):
        pending_auto=self.engine.thread.is_alive() and self.store.rows("""SELECT 1 FROM jobs j JOIN tickets t
             ON t.id=j.ticket WHERE t.permission_profile='auto' AND j.parent IS NULL AND
             j.state IN ('queued','running','waiting','draining') LIMIT 1""")
        message=("未停止的自动工单下次启动后可以继续执行并产生费用。要结束自动运行，请先停止工单。现在关闭窗口？"
                 if pending_auto else "关闭窗口会中断正在执行的任务。确定关闭吗？")
        if (self.engine.active or pending_auto) and QMessageBox.question(
                self,"关闭工作台",message,QMessageBox.StandardButton.Yes|QMessageBox.StandardButton.No,
                QMessageBox.StandardButton.No)!=QMessageBox.StandardButton.Yes:
            event.ignore()
            return
        self.timer.stop()
        self.engine.close()
        event.accept()
