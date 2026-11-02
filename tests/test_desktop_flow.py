"""Exercise the actual Qt window with a disposable store and no model calls."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
from PyQt6.QtCore import QSettings, Qt, QTimer
from PyQt6.QtTest import QTest
from PyQt6.QtWidgets import QApplication, QCheckBox, QComboBox, QDialogButtonBox, QMessageBox, QPushButton, QTextBrowser

from engine import Engine, Store
from studio import Studio


class DesktopFlowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = QApplication.instance() or QApplication([])

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        self.project = root / "工程 Ω"
        self.project.mkdir()
        self.store = Store(root / "state")
        self.addCleanup(self.store.db.close)
        settings = QSettings(str(root / "settings.ini"), QSettings.Format.IniFormat)
        self.ui = Studio(self.store, Engine(self.store, {}), settings=settings)
        self.addCleanup(self.close_window)
        self.ui.show()
        self.ui.new_chat()
        self.app.processEvents()

    def close_window(self):
        self.ui.close()
        self.ui.deleteLater()
        self.app.processEvents()

    def send(self, text):
        self.ui.editor.setPlainText(text)
        self.app.processEvents()
        self.assertTrue(self.ui.send_button.isEnabled())
        QTest.mouseClick(self.ui.send_button, Qt.MouseButton.LeftButton)
        self.app.processEvents()

    def test_single_send_creates_then_follows_in_selected_project(self):
        self.ui.set_project(str(self.project))
        goal = "正常入口\n混合 Ω🙂   "
        self.send(goal)
        tid = self.ui.tid
        self.assertIsNotNone(tid)
        follow = "继续上一轮\n  保留空格"
        self.send(follow)
        self.assertEqual(self.ui.tid, tid)
        tickets = self.store.rows("SELECT id,cwd,goal FROM tickets")
        self.assertEqual(tickets, [{"id": tid, "cwd": str(self.project), "goal": goal}])
        payload = self.store.rows("SELECT payload FROM events WHERE ticket=? AND kind='user.follow'", (tid,))
        self.assertEqual([json.loads(r["payload"]) for r in payload], [follow])
        self.assertEqual(self.ui.editor.toPlainText(), "")

    def test_new_chat_can_start_without_a_project(self):
        self.ui.editor.setPlainText("回答一个问题，不必访问项目")
        QTest.keyClick(self.ui.editor, Qt.Key.Key_Return, Qt.KeyboardModifier.ControlModifier)
        self.app.processEvents()
        row, = self.store.rows("SELECT cwd,writes FROM tickets")
        self.assertTrue(Path(row["cwd"]).is_dir())
        self.assertNotEqual(Path(row["cwd"]).resolve(), Path.cwd().resolve())
        self.assertEqual(row["writes"], 0)

    def select_permission(self, key):
        picker = self.ui.permission_picker
        picker.showPopup()
        index = picker.model().index(picker.findData(key), 0)
        point = picker.view().visualRect(index).center()
        QTest.mouseClick(picker.view().viewport(), Qt.MouseButton.LeftButton, pos=point)
        self.app.processEvents()

    def test_inline_permission_is_visible_and_stays_with_the_original_chat(self):
        self.ui.resize(800, 570)
        self.ui.set_project(str(self.project))
        self.app.processEvents()
        picker = self.ui.permission_picker
        self.assertEqual(self.ui.permission_label.text(), "权限")
        for widget in (self.ui.permission_label, picker, self.ui.send_button):
            self.assertTrue(widget.isVisibleTo(self.ui))
            self.assertTrue(self.ui.rect().contains(widget.mapTo(self.ui, widget.rect().bottomRight())))
        self.select_permission("partial")
        self.send("实施已授权的修改")
        self.assertFalse(picker.isEnabled())
        self.assertEqual(picker.currentData(), "partial")
        self.assertIn("新建对话", self.ui.permission_hint.text())
        self.ui.new_chat()
        self.assertTrue(picker.isEnabled())
        self.assertEqual(picker.currentData(), "readonly")
        self.send("只审查这个项目")
        rows = self.store.rows("SELECT writes FROM tickets ORDER BY id")
        self.assertEqual([r["writes"] for r in rows], [1, 0])

    def test_high_risk_acknowledgement_and_new_ticket_reset(self):
        observations = []
        def choose(key, grant):
            def answer():
                dialog = self.app.activeModalWidget()
                cancel = dialog.button(QMessageBox.StandardButton.Cancel)
                observations.append(("免责声明" in dialog.informativeText(), dialog.defaultButton() is cancel))
                if grant:
                    next(b for b in dialog.buttons() if dialog.buttonRole(b) == QMessageBox.ButtonRole.AcceptRole).click()
                else:
                    cancel.click()
            QTimer.singleShot(30, answer)
            self.select_permission(key)

        choose("full", False)
        self.assertEqual(self.ui.permission_picker.currentData(), "readonly")
        choose("auto", True)
        self.assertEqual(observations, [(True,True),(True,True)])
        self.assertIn("高危", self.ui.permission_picker.currentText())
        self.send("autonomous")
        self.ui.new_chat()
        self.send("read only")
        profiles=self.store.rows("SELECT permission_profile FROM tickets ORDER BY id")
        self.assertEqual([r["permission_profile"] for r in profiles],["auto","readonly"])

    def test_delegation_is_explicit_and_resets_for_each_new_ticket(self):
        recorded = []
        def opt_out():
            dialog = self.app.activeModalWidget()
            switch = dialog.findChild(QCheckBox, "allowLeadRouting")
            recorded.append(switch.isChecked())
            switch.setChecked(False)
            dialog.findChild(QDialogButtonBox).button(QDialogButtonBox.StandardButton.Ok).click()
        QTimer.singleShot(0, opt_out)
        self.ui.mode_button.click()
        self.send("固定协作配置")
        self.ui.new_chat()
        self.send("自主选择")
        rows = self.store.rows("SELECT allow_lead_routing FROM tickets ORDER BY id")
        self.assertEqual(recorded, [True])
        self.assertEqual([r["allow_lead_routing"] for r in rows], [0, 1])

    def test_job_detail_shows_effective_profile_and_bound_route(self):
        tid = self.store.create("review", self.project, "deepseek", permission_profile="partial")
        job, = self.store.rows("SELECT * FROM jobs WHERE ticket=?", (tid,))
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?", (job["id"],))
        jid = self.store.dispatch(job, {"id":"peer","to":"claude","body":"review test",
                                        "permission_profile":"readonly"})
        chosen = {"provider":"configured","model":"selected-model","mode":"cli"}
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET route_override=? WHERE id=?", (json.dumps(chosen), jid))
        self.ui.open_chat(tid)
        self.ui.jobs_list.setCurrentRow(1)
        self.assertIn("只读", self.ui.job_text.toPlainText())
        self.assertIn("主控指定", self.ui.job_text.toPlainText())
        self.assertIn("selected-model", self.ui.job_text.toPlainText())
        with self.store.tx() as db:
            db.execute("INSERT INTO native_routes(job,route) VALUES(?,?)",
                       (jid, json.dumps({**chosen,"model":"bound-model"})))
        self.ui.refresh()
        self.assertIn("执行时绑定", self.ui.job_text.toPlainText())
        self.assertIn("bound-model", self.ui.job_text.toPlainText())
        self.assertNotIn("selected-model", self.ui.job_text.toPlainText())

    def test_approval_ui_targets_only_the_pending_request(self):
        tid=self.store.create("approval",self.project,"codex",permission_profile="ask")
        job=self.store.rows("SELECT id FROM jobs WHERE ticket=?",(tid,))[0]["id"]
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?",(job,))
            db.execute("INSERT INTO approvals(ticket,job,turn,action) VALUES(?,?,1,?)",
                       (tid,job,json.dumps({"command":"test-command"})))
        self.ui.refresh()
        self.assertTrue(self.ui.approval_button.isVisible())
        def vote():
            dlg=self.app.activeModalWidget()
            from PyQt6.QtWidgets import QPlainTextEdit
            self.assertIn("test-command",dlg.findChild(QPlainTextEdit).toPlainText())
            next(b for b in dlg.findChildren(QPushButton) if b.text()=="允许本次申请").click()
        QTimer.singleShot(0,vote)
        self.ui.approval_button.click()
        self.assertEqual(self.store.rows("SELECT decision FROM approvals")[0]["decision"],"allow")
        self.assertFalse(self.ui.approval_button.isVisible())

    def test_drafts_survive_switch_and_target_does_not_leak(self):
        a = self.store.create("第一项", self.project, "deepseek")
        b = self.store.create("第二项", self.project, "codex")
        self.ui.refresh()
        self.ui.open_chat(a)
        job, = self.store.rows("SELECT id FROM jobs WHERE ticket=?", (a,))
        self.ui.target_job(job["id"])
        message = "只给这个执行者\nΩ🙂  "
        self.send(message)
        with self.store.tx() as db:
            notes = self.store.notes(db, job["id"])
        self.assertEqual([n["body"] for n in notes], [message])
        self.ui.editor.setPlainText("甲的草稿")
        self.ui.open_chat(b)
        self.assertEqual(self.ui.editor.toPlainText(), "")
        self.send("乙继续")
        self.ui.editor.setPlainText("乙的草稿")
        self.ui.open_chat(a)
        self.assertEqual(self.ui.editor.toPlainText(), "甲的草稿")
        self.ui.open_chat(b)
        self.assertEqual(self.ui.editor.toPlainText(), "乙的草稿")
        self.assertEqual(len(self.store.rows("SELECT seq FROM mailbox")), 1)
        events = self.store.rows("SELECT ticket,payload FROM events WHERE kind='user.follow'")
        self.assertEqual([(e["ticket"], json.loads(e["payload"])) for e in events], [(b, "乙继续")])

    def test_final_reply_is_readable_once_without_native_events(self):
        tid = self.store.create("汇总检查", self.project, "deepseek")
        job, = self.store.rows("SELECT * FROM jobs WHERE ticket=?", (tid,))
        body = "输入文件保持完整。"
        raw = 'SA_ROLE {"role":"检查原文"}\n\n' + body + "  \n"
        with self.store.tx() as db:
            self.store.log(db, job, "native", {"diagnostic": "RAW_NATIVE_ONLY"})
            for part in ("输入文件", "保持完整。"):
                self.store.log(db, job, "delta", part)
            db.execute("UPDATE jobs SET state='done',result=? WHERE id=?", (raw, job["id"]))
            self.store.log(db, job, "reply", {"text": raw, "state": "done", "session": "test"})
        self.ui.refresh()
        self.ui.open_chat(tid)
        self.ui.refresh()
        self.app.processEvents()
        texts = self.ui.feed.findChildren(QTextBrowser)
        if isinstance(self.ui.feed, QTextBrowser):
            texts.append(self.ui.feed)
        visible = "\n".join(w.toPlainText() for w in texts if w.isVisibleTo(self.ui))
        self.assertEqual(visible.count(body), 1)
        self.assertNotIn("RAW_NATIVE_ONLY", visible)
        self.assertNotIn("SA_ROLE", visible)
        final_view, = [w for w in texts if body in w.toPlainText() and w.isVisibleTo(self.ui)]
        copy_button, = final_view.parentWidget().findChildren(QPushButton)
        # Observe the payload without modifying the operator's clipboard.
        with patch.object(QApplication, "clipboard") as clipboard:
            copy_button.click()
            clipboard.return_value.setText.assert_called_once_with(raw)
