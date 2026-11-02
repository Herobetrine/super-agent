"""Native provider/model catalog and routing controls. Only identifiers leave this dialog."""
import threading
import time
from pathlib import Path

from PyQt6.QtCore import Qt, QTimer, QUrl, pyqtSignal
from PyQt6.QtGui import QDesktopServices
from PyQt6.QtWidgets import (
    QComboBox,QDialog,QDialogButtonBox,QFormLayout,QFrame,QHBoxLayout,QLabel,
    QPushButton,QScrollArea,QStackedWidget,QTabBar,QVBoxLayout,QWidget,
)

from studio_widgets import button,label

# Also available if the runtime is missing or its discovery fails.
SUPPORT = {
    "deepseek":("DeepSeek Harness","https://www.deepseek.com/zh/harness/",
                "https://github.com/deepseek-ai/deepseek-harness#run-from-npm"),
    "codex":("Codex","https://learn.chatgpt.com/docs/app/windows","https://learn.chatgpt.com/docs/cli"),
    "claude":("Claude Code","https://claude.com/download","https://code.claude.com/docs/en/quickstart"),
    "antigravity":("Antigravity","https://antigravity.google/download","https://antigravity.google/download"),
}


def by_id(rows,key):
    return next((row for row in rows if row["id"]==key),{})


def fill(combo,rows,current):
    combo.clear()
    first=-1
    for row in rows:
        enabled=row.get("available",True)
        title=row.get("label") or row["id"]
        combo.addItem(title+("" if enabled else "  ·  不可用"),row["id"])
        idx=combo.count()-1
        item=combo.model().item(idx)
        item.setEnabled(enabled)
        item.setToolTip(row.get("detail","") or row["id"])
        if enabled and first<0:
            first=idx
    idx=combo.findData(current)
    if idx<0 and current:
        combo.addItem(str(current)+"  ·  未在当前目录找到",current)
        idx=combo.count()-1
        combo.model().item(idx).setEnabled(False)
    combo.setCurrentIndex(idx if idx>=0 else first)


class AgentPage(QWidget):
    def __init__(self,agent,saved,locked):
        super().__init__()
        self.setObjectName("catalogPage")
        self.agent,self.saved,self.locked=agent,saved,locked
        self.catalog={}
        self.touched=False
        outer=QVBoxLayout(self)
        outer.setContentsMargins(5,10,5,6)
        outer.setSpacing(9)
        self.source=label("正在读取本机配置…","hint",wrap=True)
        outer.addWidget(self.source)
        form=QFormLayout()
        form.setHorizontalSpacing(16)
        form.setVerticalSpacing(8)
        self.mode,self.provider,self.model=(QComboBox() for _ in range(3))
        for name,field in [("执行端",self.mode),("供应商",self.provider),("模型",self.model)]:
            field.setAccessibleName(SUPPORT[agent][0]+" "+name)
            field.setMinimumWidth(200)
            field.setSizeAdjustPolicy(QComboBox.SizeAdjustPolicy.AdjustToMinimumContentsLengthWithIcon)
            form.addRow(name,field)
            field.setEnabled(False)
        outer.addLayout(form)
        self.state=label("","speaker",wrap=True)
        outer.addWidget(self.state)
        self.limit=label("","micro",wrap=True)
        self.limit.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        outer.addWidget(self.limit)
        actions=QHBoxLayout()
        self.pin=button("使用当前组合",self.choose,style="chip")
        self.reset=button("跟随适配器默认",self.use_default)
        actions.addWidget(self.pin)
        actions.addWidget(self.reset)
        actions.addStretch()
        outer.addLayout(actions)
        self.pin.setEnabled(False)
        self.reset.setEnabled(False)
        line=QFrame(objectName="divider")
        outer.addWidget(line)
        outer.addWidget(label("官方下载与安装","speaker"))
        self.links=QHBoxLayout()
        outer.addLayout(self.links)
        self.downloads([])
        outer.addStretch()
        self.mode.activated.connect(lambda _:self.changed("mode"))
        self.provider.activated.connect(lambda _:self.changed("provider"))
        self.model.activated.connect(lambda _:self.changed("model"))

    def downloads(self,links):
        while self.links.count():
            item=self.links.takeAt(0)
            if item.widget():
                item.widget().deleteLater()
        if not links:
            _,gui,cli=SUPPORT[self.agent]
            links=[{"label":"桌面端下载","url":gui},{"label":"CLI 安装指南","url":cli}]
        for link in links:
            url=QUrl(link["url"])
            if url.scheme()!="https":
                continue
            control=button(link["label"]+" ↗",lambda _,u=url:QDesktopServices.openUrl(u),style="chip")
            control.setToolTip(url.toString())
            self.links.addWidget(control)
        self.links.addStretch()

    def load(self,catalog):
        if catalog==self.catalog and self.catalog:
            return
        self.catalog=catalog
        self.downloads(catalog.get("downloads",[]))
        self.source.setText(catalog.get("detail",""))
        choice=self.saved if self.saved is not None else catalog.get("default",{})
        self.populate(choice)
        self.describe()

    def populate(self,choice):
        modes=self.catalog.get("modes",[])
        fill(self.mode,modes,choice.get("mode"))
        active_mode=by_id(modes,self.mode.currentData())
        providers=self.catalog.get("providers",[])
        if "providers" in active_mode:
            providers=[p for p in providers if p["id"] in active_mode["providers"]]
        fill(self.provider,providers,choice.get("provider"))
        provider=by_id(providers,self.provider.currentData())
        fill(self.model,provider.get("models",[]),choice.get("model"))
        for field in (self.mode,self.provider,self.model):
            field.setEnabled(not self.locked and field.count()>0)

    def selected(self):
        return dict(mode=self.mode.currentData(),provider=self.provider.currentData(),model=self.model.currentData())

    def valid(self):
        return all(c.currentIndex()>=0 and c.model().item(c.currentIndex()).isEnabled()
                   for c in (self.mode,self.provider,self.model))

    def describe(self):
        route=self.selected()
        scope="已固定" if self.saved is not None else "跟随默认"
        modes=self.catalog.get("modes",[])
        provider=by_id(self.catalog.get("providers",[]),route["provider"])
        self.state.setText(f"{scope}   ·   {provider.get('label') or route['provider'] or '—'}  /  {route['model'] or '—'}")
        details=[p for p in (provider.get("detail"),) if p]
        unavailable=[p for p in self.catalog.get("providers",[]) if not p.get("available",True)]
        if unavailable:
            details.extend(p.get("label",p["id"])+" · "+(p.get("detail") or "此配置不可自动执行") for p in unavailable)
        details += [m["label"]+"   "+m.get("detail","") for m in modes if not m.get("available")]
        if not self.valid() and self.saved:
            details.insert(0,"已保存的选择目前不可用。可重新选择或恢复默认。")
        self.limit.setText("\n".join(details))
        self.pin.setEnabled(not self.locked and self.valid())
        self.reset.setEnabled(not self.locked and self.saved is not None)

    def choose(self):
        if self.valid() and not self.locked:
            self.touched=True
            self.saved=self.selected()
        self.describe()

    def changed(self,which):
        choice=self.selected()
        if which=="mode":
            choice["provider"]=choice["model"]=None
        elif which=="provider":
            default=self.catalog.get("default",{})
            choice["model"]=default.get("model") if choice["provider"]==default.get("provider") else None
        self.populate(choice)
        self.saved=self.selected()
        self.touched=True
        self.describe()

    def use_default(self):
        self.saved=None
        self.touched=True
        self.populate(self.catalog.get("default",{}))
        self.describe()


class AgentSettings(QDialog):
    result_ready=pyqtSignal(str,object)

    def __init__(self,adapters,selections,parent=None,*,ticket=None,locked=False,initial="deepseek",legacy_route=None,draft=False):
        super().__init__(parent)
        self.setWindowTitle("Agent 配置")
        self.resize(660,550)
        self.setMinimumSize(530,440)
        self.adapters,self.pages,self.pending=adapters,{},set()
        self.stamps,self.last_read,self.dirty={},{},set()
        self.alive=True
        layout=QVBoxLayout(self)
        layout.setContentsMargins(20,16,20,15)
        layout.setSpacing(8)
        titlebar=QHBoxLayout()
        titlebar.addWidget(label("Agent 配置","title"),1)
        self.reload=button("重新读取",self.discover,style="chip",glyph="pulse")
        titlebar.addWidget(self.reload)
        layout.addLayout(titlebar)
        scope=("本次新对话" if draft else "新对话默认设置") if ticket is None else f"当前对话 #{ticket}"
        desc="任务进行中，仅供查看。" if locked else ("保存后对新建任务生效。" if ticket is None else
                    "更换路由后，相关 agent 将用新原生会话接续现有工单。")
        layout.addWidget(label(scope+"  ·  "+desc,"micro",wrap=True))
        self.tabs=QTabBar()
        self.tabs.setObjectName("catalogTabs")
        self.tabs.setAccessibleName("选择 agent")
        self.stack=QStackedWidget()
        for name,(title,_,_) in SUPPORT.items():
            self.tabs.addTab(title)
            self.tabs.setTabData(self.tabs.count()-1,name)
            page=AgentPage(name,selections.get(name),locked)
            scroll=QScrollArea()
            scroll.setWidgetResizable(True)
            scroll.setWidget(page)
            self.stack.addWidget(scroll)
            self.pages[name]=page
        layout.addWidget(self.tabs)
        layout.addWidget(self.stack,1)
        self.legacy_route=legacy_route
        self.tabs.currentChanged.connect(self.stack.setCurrentIndex)
        self.tabs.setCurrentIndex(list(SUPPORT).index(initial))
        self.notice=label("这里只保存供应商、模型和执行端标识。","micro",wrap=True)
        layout.addWidget(self.notice)
        buttons=QDialogButtonBox(QDialogButtonBox.StandardButton.Close if locked else
                                 QDialogButtonBox.StandardButton.Save|QDialogButtonBox.StandardButton.Cancel)
        for role,caption in ((QDialogButtonBox.StandardButton.Close,"关闭"),(QDialogButtonBox.StandardButton.Save,"保存设置"),
                             (QDialogButtonBox.StandardButton.Cancel,"取消")):
            control=buttons.button(role)
            if control:
                control.setText(caption)
        buttons.rejected.connect(self.reject)
        buttons.accepted.connect(self.save)
        layout.addWidget(buttons)
        self.result_ready.connect(self.loaded)
        self.finished.connect(self.closed)
        self.watcher=QTimer(self)
        self.watcher.setInterval(1250)
        self.watcher.timeout.connect(self.poll_sources)
        for name in SUPPORT:
            self.stamps[name]=self.fingerprint(name)
        self.discover()
        self.watcher.start()

    def closed(self,*_):
        self.alive=False
        self.watcher.stop()

    def fingerprint(self,name):
        adapter=self.adapters.get(name)
        try:
            paths=getattr(adapter,"watch_paths",lambda:())()
        except (OSError,ValueError,RuntimeError):
            return ()
        values=[]
        for p in sorted({Path(path) for path in paths},key=str):
            try:
                info=p.stat()
                values.append((str(p),info.st_size,info.st_mtime_ns,info.st_ino))
            except OSError:
                values.append((str(p),None))
        return tuple(values)

    def poll_sources(self):
        now=time.monotonic()
        for name in SUPPORT:
            try:
                stamp=self.fingerprint(name)
            except (OSError,ValueError,RuntimeError):
                stamp=()
            prior=self.stamps.setdefault(name,stamp)
            self.stamps[name]=stamp
            if prior!=stamp:
                self.dirty.add(name)
            # Also refresh native model catalogs whose remote changes have no file event.
            if name not in self.pending and (name in self.dirty or now-self.last_read.get(name,now)>=60):
                self.read(name)

    def discover(self):
        for name in SUPPORT:
            if name not in self.pending:
                self.read(name)

    def read(self,name):
        self.pending.add(name)
        self.dirty.discard(name)
        self.reload.setEnabled(False)
        def worker():
            try:
                data=self.adapters[name].choices()
            except Exception as exc:
                data={"detail":"此入口未能读取配置（"+type(exc).__name__+"）。可重新读取或使用官方安装指南。"}
            if self.alive:
                try:
                    self.result_ready.emit(name,data)
                except RuntimeError:
                    pass
        threading.Thread(target=worker,daemon=True,name="catalog-"+name).start()

    def loaded(self,name,data):
        if not self.alive:
            return
        if name=="claude" and self.legacy_route=="user" and self.pages[name].saved is None:
            # Older tickets pin the legacy CLI route, independent of current global preferences.
            old=data.get("legacy_defaults",{}).get("user",{})
            data={**data,"default":old,
                  "detail":"该对话沿用历史 CLI 路由，具体模型以原生回执为准。"}
        self.pages[name].load(data)
        self.last_read[name]=time.monotonic()
        self.pending.discard(name)
        self.reload.setEnabled(not self.pending)
        if not self.pending:
            self.notice.setText("目录已更新  "+time.strftime("%H:%M:%S")+"   ·   正在监测本机配置变更")

    def save(self):
        for name,page in self.pages.items():
            if page.touched and page.saved is not None and not page.valid():
                self.tabs.setCurrentIndex(list(SUPPORT).index(name))
                self.notice.setText("该组合不可自动执行，请更换选择。")
                return
        self.accept()

    def selections(self):
        return {name:page.saved for name,page in self.pages.items() if page.saved is not None}
