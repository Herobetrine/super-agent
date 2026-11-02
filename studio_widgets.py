"""Small presentation components for the native desktop client."""
from pathlib import Path

from PyQt6.QtCore import QByteArray, QSize, Qt, pyqtSignal
from PyQt6.QtGui import QColor, QDesktopServices, QFontMetrics, QIcon, QPainter, QPixmap, QTextDocument
from PyQt6.QtSvg import QSvgRenderer
from PyQt6.QtWidgets import QLabel, QPlainTextEdit, QPushButton, QSizePolicy, QStyle, QStyledItemDelegate, QTextBrowser

ASSETS = Path(__file__).resolve().parent / "assets"
GLYPHS = {
    "plus": "M12 5v14M5 12h14",
    "arrow": "M12 19V5m-6 6 6-6 6 6",
    "folder": "M3 7V5h6l3 3h9v11H3z",
    "sliders": "M4 7h7m4 0h5M4 17h3m4 0h9M11 4v6M7 14v6",
    "team": "M7 5a3 3 0 1 0 .1 0M2 19v-2c0-4 10-4 10 0v2M15 7a2.5 2.5 0 1 1 0 5m0 3c4 0 6 1 6 4",
    "pulse": "M2 12h4l3-7 5 14 3-7h5",
    "book": "M4 4h7l1 2 1-2h7v14h-6l-2 2-2-2H4zM12 6v14",
    "code": "m8 6-6 6 6 6m8-12 6 6-6 6m-3-15-2 18",
    "map": "m3 7 6-3 6 3 6-3v14l-6 3-6-3-6 3zM9 4v14m6-11v14",
    "chat": "M4 4h16v12H9l-5 4z",
    "copy": "M8 8h12v13H8zM4 15V3h12",
    "menu": "M3 5h18M3 12h18M3 19h18",
    "check": "m4 12 5 5 11-12",
    "stop": "M6 6h12v12H6z",
    "close": "m6 6 12 12M18 6 6 18",
    "search": "M10 3a7 7 0 1 0 .1 0m5 12 6 6",
    "activity": "M3 6h6m6 0h6M9 6l6 12h6",
    "dots": "M5 12h.1m6.9 0h.1m6.9 0h.1",
}
ICONS = {}


def icon(name, color="#486850"):
    key = (name,color)
    if key not in ICONS:
        svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">'
               f'<path d="{GLYPHS[name]}" fill="none" stroke="{color}" stroke-width="1.7" '
               'stroke-linecap="round" stroke-linejoin="round"/></svg>').encode()
        pix = QPixmap(48,48)
        pix.fill(Qt.GlobalColor.transparent)
        painter = QPainter(pix)
        QSvgRenderer(QByteArray(svg)).render(painter)
        painter.end()
        ICONS[key] = QIcon(pix)
    return ICONS[key]


def label(text="", style="", *, wrap=False):
    obj = QLabel(text)
    obj.setTextFormat(Qt.TextFormat.PlainText)
    obj.setObjectName(style)
    obj.setWordWrap(wrap)
    return obj


def button(text, fn, *, style="", glyph=None, color="#486850", tip=""):
    obj = QPushButton(text)
    obj.setObjectName(style)
    if glyph:
        obj.setIcon(icon(glyph,color))
        obj.setIconSize(QSize(17,17))
    if tip:
        obj.setToolTip(tip)
        obj.setAccessibleName(tip)
    obj.setCursor(Qt.CursorShape.PointingHandCursor)
    obj.clicked.connect(fn)
    return obj


class Editor(QPlainTextEdit):
    submit = pyqtSignal()
    def keyPressEvent(self,event):
        if event.key() in (Qt.Key.Key_Return,Qt.Key.Key_Enter) and event.modifiers() == Qt.KeyboardModifier.ControlModifier:
            self.submit.emit()
            event.accept()
        else:
            super().keyPressEvent(event)


class ConversationRow(QStyledItemDelegate):
    def sizeHint(self,option,index):
        return QSize(120,52)

    def paint(self,painter,option,index):
        painter.save()
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        rect=option.rect.adjusted(0,1,-1,-1)
        if option.state & (QStyle.StateFlag.State_Selected|QStyle.StateFlag.State_MouseOver):
            painter.setBrush(QColor("#385143"))
            painter.setPen(Qt.PenStyle.NoPen)
            painter.drawRoundedRect(rect,7,7)
        area=rect.adjusted(9,3,-7,-3)
        font=option.font
        font.setPixelSize(13)
        painter.setFont(font)
        painter.setPen(QColor("#ecf1e8"))
        painter.drawText(area.adjusted(0,0,0,-20),Qt.AlignmentFlag.AlignVCenter,
                         QFontMetrics(font).elidedText(index.data(),Qt.TextElideMode.ElideRight,area.width()))
        font.setPixelSize(11)
        painter.setFont(font)
        painter.setPen(QColor("#b1c3b4"))
        meta=index.data(Qt.ItemDataRole.UserRole+1) or ""
        painter.drawText(area.adjusted(0,20,0,0),Qt.AlignmentFlag.AlignVCenter,
                         QFontMetrics(font).elidedText(meta,Qt.TextElideMode.ElideRight,area.width()))
        painter.restore()


class MessageText(QTextBrowser):
    """Selectable rich text that never loads embedded resources from replies."""
    def __init__(self):
        super().__init__()
        self.setOpenLinks(False)
        self.setOpenExternalLinks(False)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.setSizePolicy(QSizePolicy.Policy.Expanding,QSizePolicy.Policy.Fixed)
        self.document().setDocumentMargin(0)
        self.anchorClicked.connect(self.open_link)

    def loadResource(self,kind,url):
        return QByteArray()

    def open_link(self,url):
        if url.scheme() in ("http","https"):
            QDesktopServices.openUrl(url)

    def put(self,text,markdown):
        if markdown:
            features=QTextDocument.MarkdownFeature
            self.document().setMarkdown(text,features.MarkdownDialectGitHub|features.MarkdownNoHTML)
        else:
            self.setPlainText(text)
        self.fit()

    def fit(self):
        self.document().setTextWidth(max(50,self.viewport().width()))
        self.setFixedHeight(max(23,int(self.document().size().height())+5))

    def resizeEvent(self,event):
        super().resizeEvent(event)
        if event.oldSize().width()!=event.size().width():
            self.fit()


def public_text(text):
    return "\n".join(line for line in text.split("\n")
                     if not line.lstrip().startswith(("SA_SEND ","SA_NOTE ","SA_ROLE ","SA_GOAL ","SA_NEXT ",
                                                      "SA_FINISH ","SA_LEARN ","SA_EXHAUSTED "))).strip()
