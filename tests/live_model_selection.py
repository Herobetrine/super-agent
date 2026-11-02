"""Optional real provider/model check through Qt. Uses an isolated database and project."""
import argparse
import json
from pathlib import Path
import secrets
import sys
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from PyQt6.QtCore import QSettings,QTimer,Qt
from PyQt6.QtTest import QTest
from PyQt6.QtWidgets import QApplication,QDialogButtonBox
from agent_settings import AgentSettings,SUPPORT
from engine import Engine,Store,TERMINAL
from studio import Studio


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--agent",choices=tuple(SUPPORT),required=True)
    parser.add_argument("--provider",required=True)
    parser.add_argument("--model",required=True)
    args=parser.parse_args()
    base=Path.home()/".local/share/superagent/checks"/("routes-"+secrets.token_hex(5))
    project=base/"project"
    project.mkdir(parents=True)
    nonce=secrets.token_hex(9)
    (project/"challenge.txt").write_bytes(nonce.encode())
    app=QApplication([])
    store=Store(base/"state")
    engine=Engine(store,timeout=220)
    ui=Studio(store,engine,settings=QSettings(str(base/"ui.ini"),QSettings.Format.IniFormat))
    ui.show()
    ui.set_project(project)
    ui.lead=args.agent
    failures=[]
    def pick():
        dialog=app.activeModalWidget()
        if not isinstance(dialog,AgentSettings) or dialog.pending:
            QTimer.singleShot(90,pick)
            return
        try:
            page=dialog.pages[args.agent]
            dialog.tabs.setCurrentIndex(list(SUPPORT).index(args.agent))
            for control,value in [(page.provider,args.provider),(page.model,args.model)]:
                idx=control.findData(value)
                if idx<0 or not control.model().item(idx).isEnabled():
                    raise RuntimeError("选择在当前本机目录中不可用")
                control.setCurrentIndex(idx)
                control.activated.emit(idx)
            dialog.findChild(QDialogButtonBox).button(QDialogButtonBox.StandardButton.Save).click()
        except Exception as exc:
            failures.append(str(exc))
            dialog.reject()

    try:
        QTimer.singleShot(90,pick)
        ui.configure_defaults()
        if failures:
            raise RuntimeError(failures[0])
        selected=dict(provider=args.provider,model=args.model,mode="cli")
        assert store.get_routes()[args.agent]==selected
        ui.editor.setPlainText("读取当前项目的 challenge.txt，原样返回其中的字符串。")
        engine.start()
        QTest.mouseClick(ui.send_button,Qt.MouseButton.LeftButton)
        tid=ui.tid
        assert store.get_routes(tid)[args.agent]==selected
        end=time.monotonic()+250
        while time.monotonic()<end:
            app.processEvents()
            row=store.rows("SELECT id,state,result FROM jobs WHERE ticket=? AND parent IS NULL",(tid,))[0]
            if row["state"] in TERMINAL:
                break
            time.sleep(.08)
        events=store.rows("SELECT kind,payload FROM events WHERE job=? AND kind IN ('route.requested','route.selected','adapter.route') ORDER BY seq",(row["id"],))
        okay=(row["state"]=="done" and nonce in (row["result"] or "") and
              any(x["kind"]=="route.requested" and json.loads(x["payload"])==selected for x in events))
        receipt=dict(ticket=tid,selection=selected,state=row["state"],file_challenge=nonce in (row["result"] or ""),
                     routing=[{"kind":x["kind"],"payload":json.loads(x["payload"])} for x in events],passed=okay)
        (base/"receipt.json").write_text(json.dumps(receipt,ensure_ascii=False,indent=2),encoding="utf-8")
        store.export(tid,base/"events.jsonl")
        print(base,receipt,flush=True)
        return 0 if okay else 1
    finally:
        if ui.tid and engine.active:
            engine.cancel(ui.tid)
        engine.close()
        ui.close()
        store.db.close()

if __name__=="__main__":
    raise SystemExit(main())
