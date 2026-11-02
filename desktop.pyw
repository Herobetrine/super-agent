"""Launch the single-owner Windows desktop workspace."""
import ctypes
import os
import sys
from pathlib import Path

from PyQt6.QtGui import QFont
from PyQt6.QtWidgets import QApplication, QMessageBox

from engine import Engine, Store
from host import claim
from studio import Studio, TITLE


def main():
    data=Path(os.environ.get("SUPERAGENT_DATA") or Path.home()/".local/share/superagent/control-room")
    data.mkdir(parents=True,exist_ok=True)
    if not claim(data,TITLE):
        return
    ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("SuperAgent.LocalStudio")
    app=QApplication(sys.argv)
    app.setApplicationName("Super Agent")
    app.setStyle("Fusion")
    app.setFont(QFont("Microsoft YaHei UI",10))
    try:
        store=Store(data)
        engine=Engine(store)
        window=Studio(store,engine)
        window.show()
        engine.start()
        app.exec()
        store.db.close()
    except Exception as exc:
        QMessageBox.critical(None,"Super Agent",str(exc))
        raise


if __name__=="__main__":
    main()
