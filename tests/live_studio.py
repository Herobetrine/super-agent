"""Paid, optional round trips through the same Qt send button and native scheduler."""
import json
import os
from pathlib import Path
import secrets
import sys
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from PyQt6.QtCore import QSettings, Qt
from PyQt6.QtGui import QFont
from PyQt6.QtTest import QTest
from PyQt6.QtWidgets import QApplication

from engine import Engine, Store, TERMINAL
from studio import Studio


def main():
    base=Path.home()/".local/share/superagent/checks"/("studio-"+secrets.token_hex(4))
    project=base/"project"
    project.mkdir(parents=True)
    token=secrets.token_hex(7)
    (project/"sample.txt").write_text(token,encoding="utf-8")
    (project/"usage.txt").write_text("east=124\nwest=73\nsouth=48\n",encoding="utf-8")
    app=QApplication([])
    app.setStyle("Fusion")
    app.setFont(QFont("Microsoft YaHei UI",10))
    store=Store(base/"state")
    engine=Engine(store,timeout=240)
    ui=Studio(store,engine,settings=QSettings(str(base/"ui.ini"),QSettings.Format.IniFormat))
    ui.show()
    engine.start()
    receipts=[]
    goals=["读取 sample.txt，把文件中的字符串原样回复给我。",
           "请核对 usage.txt 的三区用量合计；这项核对需要两份出自不同执行者的独立计算结果。请自行安排分工，最终给我合计和核对结论。"]
    try:
        for index,goal in enumerate(goals):
            ui.new_chat()
            ui.set_project(project)
            ui.editor.setPlainText(goal)
            QTest.mouseClick(ui.send_button,Qt.MouseButton.LeftButton)
            tid=ui.tid
            print(f"真实对话 #{tid}  {base}",flush=True)
            deadline=time.monotonic()+360
            last=None
            while time.monotonic()<deadline:
                app.processEvents()
                jobs=store.rows("SELECT id,agent,role,state,result FROM jobs WHERE ticket=? ORDER BY id",(tid,))
                states=[(j["agent"],j["role"],j["state"]) for j in jobs]
                if states!=last:
                    print(states,flush=True)
                    last=states
                if jobs[0]["state"] in TERMINAL or jobs[0]["state"]=="interrupted":
                    break
                time.sleep(.08)
            if jobs[0]["state"] not in TERMINAL:
                engine.cancel(tid)
            ui.refresh()
            app.processEvents()
            store.export(tid,base/f"events-{tid}.jsonl")
            native_route=store.rows("SELECT payload FROM events WHERE ticket=? AND kind='adapter.route' ORDER BY seq",(tid,))
            body=jobs[0]["result"] or ""
            okay=(jobs[0]["state"]=="done" and bool(native_route) and
                  (len(jobs)==1 and token in body if index==0 else len(jobs)>1 and "245" in body))
            receipts.append(dict(ticket=tid,jobs=jobs,route=[json.loads(r["payload"]) for r in native_route],passed=okay))
            ui.grab().save(str(base/f"conversation-{tid}.png"))
        (base/"receipt.json").write_text(json.dumps(receipts,ensure_ascii=False,indent=2),encoding="utf-8")
        print("passed", [r["passed"] for r in receipts],base,flush=True)
        return 0 if all(r["passed"] for r in receipts) else 1
    finally:
        engine.close()
        ui.close()
        store.db.close()


if __name__=="__main__":
    raise SystemExit(main())
