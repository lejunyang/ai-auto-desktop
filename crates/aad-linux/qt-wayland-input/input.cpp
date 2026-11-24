// Opt-in Qt Widgets generic plugin. All checks and both event phases run
// on the target application's GUI thread, addressed to the observed QWidget.
#include <QAccessible>
#include <QApplication>
#include <QDBusConnection>
#include <QDragEnterEvent>
#include <QGenericPlugin>
#include <QKeyEvent>
#include <QLineEdit>
#include <QListWidget>
#include <QMimeData>
#include <QMouseEvent>
#include <QPointer>
#include <QScopedValueRollback>
#include <QTimer>
#include <QWindow>
#include <climits>
#include <ctime>
#include <memory>
#include <typeinfo>

namespace {
QAccessibleInterface *accessibleFor(const QString &path) {
    static const QString prefix = QStringLiteral("/org/a11y/atspi/accessible/");
    if (!path.startsWith(prefix)) return nullptr;
    bool ok = false;
    const auto id = path.mid(prefix.size()).toUInt(&ok);
    if (!ok || id <= INT_MAX || prefix + QString::number(id) != path) return nullptr;
    auto *iface = QAccessible::accessibleInterface(id);
    if (!iface || !iface->isValid()) return nullptr;
    const auto state = iface->state();
    return state.passwordEdit || state.disabled || state.invisible || state.offscreen ? nullptr : iface;
}

QWidget *widgetFor(const QString &path) {
    auto *iface = accessibleFor(path);
    if (!iface) return nullptr;
    auto *widget = qobject_cast<QWidget *>(iface->object());
    // Virtual accessible children may share a backing QObject.
    if (!widget || QAccessible::queryAccessibleInterface(widget) != iface) return nullptr;
    if (auto *edit = qobject_cast<QLineEdit *>(widget); edit && edit->echoMode() != QLineEdit::Normal)
        return nullptr;
    return widget;
}

QWidget *inWindow(QWidget *widget, const QString &window, bool requireActive) {
    auto *top = widgetFor(window);
    if (!widget || !top || !top->isWindow() || widget->window() != top ||
        !widget->isEnabled() || !widget->isVisible() || top->isMinimized() ||
        !top->windowHandle() || !top->windowHandle()->isExposed() ||
        (requireActive && (QApplication::activeWindow() != top ||
                           QGuiApplication::focusWindow() != top->windowHandle())) ||
        QApplication::activePopupWidget() || QWidget::mouseGrabber() || QWidget::keyboardGrabber())
        return nullptr;
    auto *modal = QApplication::activeModalWidget();
    return modal && modal != top ? nullptr : widget;
}

QWidget *boundWidget(const QString &target, const QString &window, bool requireActive = true) {
    return inWindow(widgetFor(target), window, requireActive);
}

bool focused(QWidget *widget) {
    return widget->hasFocus() && QApplication::focusWidget() == widget;
}

bool hits(QWidget *widget, QPoint point) {
    auto *top = widget->window();
    const QPoint local = widget->mapFromGlobal(point);
    if (!widget->visibleRegion().contains(local)) return false;
    auto *hit = top->childAt(top->mapFromGlobal(point));
    return (hit ? hit : top) == widget;
}

// A subclass can replace startDrag or the view's drop policy even without Q_OBJECT.
QListView *standardList(QWidget *widget) {
    if (!widget || (typeid(*widget) != typeid(QListView) && typeid(*widget) != typeid(QListWidget)))
        return nullptr;
    return static_cast<QListView *>(widget);
}

struct ListItem {
    QPointer<QListView> view;
    QPersistentModelIndex index;
};

ListItem selectedItemAt(const QString &path, const QString &window, const QPoint &point) {
    auto *item = accessibleFor(path);
    if (!item || item->role() != QAccessible::ListItem || !item->rect().contains(point)) return {};
    auto *cell = item->tableCellInterface();
    auto *parent = item->parent();
    auto *view = parent ? standardList(qobject_cast<QWidget *>(parent->object())) : nullptr;
    if (!cell || !view || !inWindow(view, window, true) ||
        QAccessible::queryAccessibleInterface(view) != parent ||
        !hits(view->viewport(), point) || parent->childAt(point.x(), point.y()) != item ||
        !view->dragEnabled() || view->dragDropMode() == QAbstractItemView::InternalMove ||
        !view->model() || !view->model()->supportedDragActions().testFlag(Qt::CopyAction) ||
        !view->selectionModel()) return {};
    const QModelIndex index = view->indexAt(view->viewport()->mapFromGlobal(point));
    if (!index.isValid() || index.row() != cell->rowIndex() || index.column() != cell->columnIndex() ||
        !index.flags().testFlag(Qt::ItemIsEnabled) || !index.flags().testFlag(Qt::ItemIsDragEnabled) ||
        !cell->isSelected()) return {};
    const auto ranges = view->selectionModel()->selection();
    if (ranges.size() != 1 || ranges.first().topLeft() != index || ranges.first().bottomRight() != index)
        return {};
    return {view, index};
}

// An observed list owns its viewport. Only empty viewport space belongs to
// the list itself. A row, editor or overlay needs its own observed identity.
QWidget *dropReceiver(QWidget *widget, const QPoint &point) {
    if (!widget || !widget->acceptDrops()) return nullptr;
    if (auto *list = standardList(widget)) {
        auto *viewport = list->viewport();
        if (!viewport->acceptDrops() || !hits(viewport, point) ||
            list->indexAt(viewport->mapFromGlobal(point)).isValid() ||
            list->dragDropMode() == QAbstractItemView::InternalMove || !list->model() ||
            !list->model()->supportedDropActions().testFlag(Qt::CopyAction)) return nullptr;
        return viewport;
    }
    return hits(widget, point) ? widget : nullptr;
}

// Materialize the model's advertised wire formats while the original index
// is pinned. Never hand a receiver a model-owned object or lazy item pointer.
bool copyItemMime(const ListItem &item, QMimeData &payload) {
    auto *model = item.index.model();
    if (!model) return false;
    const QStringList declared = model->mimeTypes();
    if (!item.index.isValid()) return false;
    std::unique_ptr<QMimeData> mime(model->mimeData({item.index}));
    if (!mime) return false;
    const QStringList formats = mime->formats();
    if (formats.isEmpty() || formats.size() > 16) return false;
    int remaining = 1024 * 1024;
    for (const QString &format : formats) {
        if (format.isEmpty() || !declared.contains(format) || payload.hasFormat(format)) return false;
        const QByteArray bytes = mime->data(format);
        if (bytes.isEmpty() || bytes.size() > remaining) return false;
        payload.setData(format, QByteArray(bytes.constData(), bytes.size()));
        remaining -= int(bytes.size());
    }
    return true;
}

// Check every logical pixel on this bounded segment before the press, and
// again after any application code that can change geometry or cover the path.
bool lineHits(QWidget *widget, QPoint start, QPoint end) {
    const qint64 dx = qint64(end.x()) - start.x(), dy = qint64(end.y()) - start.y();
    const qint64 steps = qMax(qAbs(dx), qAbs(dy));
    if (!steps || steps > 8192) return false;
    for (qint64 i = 0; i <= steps; ++i) {
        QPoint point(start.x() + qRound64(double(dx * i) / steps),
                     start.y() + qRound64(double(dy * i) / steps));
        if (!hits(widget, point)) return false;
    }
    return true;
}

Qt::MouseButton mouseButton(int button) {
    switch (button) {
    case 1: return Qt::LeftButton;
    case 2: return Qt::MiddleButton;
    case 3: return Qt::RightButton;
    default: return Qt::NoButton;
    }
}

void sendMouse(QWidget *widget, QPoint point, QEvent::Type type, Qt::MouseButton button,
               Qt::MouseButtons held, Qt::KeyboardModifiers modifiers) {
    QMouseEvent event(type, widget->mapFromGlobal(point), widget->window()->mapFromGlobal(point),
                      point, button, held, modifiers);
    QCoreApplication::sendEvent(widget, &event);
}

bool inTime(qulonglong deadline) {
    timespec now{};
    return clock_gettime(CLOCK_MONOTONIC, &now) == 0 &&
           qulonglong(now.tv_sec) * 1000000000 + qulonglong(now.tv_nsec) < deadline;
}

class DropReceiverGuard : public QObject {
    QPointer<QWidget> receiver;
    QEvent *phase = nullptr;
    bool observed = false;
public:
    explicit DropReceiverGuard(QWidget *widget) : receiver(widget) {
        qApp->installEventFilter(this);
    }
    bool deliver(QWidget *address, QEvent &event, bool permit = true) {
        observed = false;
        phase = &event;
        if (!permit) receiver.clear();
        event.ignore();
        const bool handled = QCoreApplication::sendEvent(address, &event);
        phase = nullptr;
        return handled && observed && event.isAccepted();
    }
    bool eventFilter(QObject *object, QEvent *event) override {
        if (event != phase) return false;
        if (object == receiver) {
            observed = true;
            return false;
        }
        event->ignore();
        return true;
    }
};

struct Chord {
    Qt::KeyboardModifiers modifiers;
    int key = 0;
    QString text;
};

bool parseModifiers(const QStringList &keys, Qt::KeyboardModifiers &modifiers) {
    if (keys.size() > 4) return false;
    for (const QString &key : keys) {
        const auto name = key.toLower();
        Qt::KeyboardModifier modifier;
        if (name == "ctrl" || name == "control") modifier = Qt::ControlModifier;
        else if (name == "shift") modifier = Qt::ShiftModifier;
        else if (name == "alt") modifier = Qt::AltModifier;
        else if (name == "meta" || name == "super" || name == "win") modifier = Qt::MetaModifier;
        else return false;
        if (modifiers.testFlag(modifier)) return false;
        modifiers |= modifier;
    }
    return true;
}

bool parseChord(QStringList keys, Chord &chord) {
    if (keys.isEmpty()) return false;
    const QString main = keys.takeLast().toLower();
    if (!parseModifiers(keys, chord.modifiers)) return false;
    static const QMap<QString, int> named{
        {"backspace", Qt::Key_Backspace}, {"delete", Qt::Key_Delete}, {"enter", Qt::Key_Return},
        {"return", Qt::Key_Return}, {"esc", Qt::Key_Escape}, {"escape", Qt::Key_Escape},
        {"space", Qt::Key_Space}, {"home", Qt::Key_Home}, {"end", Qt::Key_End},
        {"left", Qt::Key_Left}, {"right", Qt::Key_Right}, {"up", Qt::Key_Up}, {"down", Qt::Key_Down},
        {"pageup", Qt::Key_PageUp}, {"pagedown", Qt::Key_PageDown}, {"tab", Qt::Key_Tab}
    };
    if (main.size() == 1 && main.at(0).unicode() < 128 && main.at(0).isLetterOrNumber()) {
        chord.key = main.toUpper().at(0).unicode();
        chord.text = chord.modifiers.testFlag(Qt::ShiftModifier) ? main.toUpper() : main;
    } else if (main.startsWith('f')) {
        const int number = main.mid(1).toInt();
        if (number < 1 || number > 35 || main != "f" + QString::number(number)) return false;
        chord.key = Qt::Key_F1 + number - 1;
    } else {
        chord.key = named.value(main);
        if (chord.key == Qt::Key_Space) chord.text = " ";
        if (chord.key == Qt::Key_Return) chord.text = "\r";
    }
    if (chord.modifiers & ~Qt::ShiftModifier) chord.text.clear();
    return chord.key != 0;
}

class Input : public QObject {
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.aiautodesktop.QtWaylandInput1")
    bool delivering = false;
public:
    Input() {
        auto *timer = new QTimer(this);
        connect(timer, &QTimer::timeout, this, [this, timer] {
            // Use Qt's existing AT-SPI connection. Its unique owner is already
            // pinned by the observed NativeRef, including across app restarts.
            QDBusConnection bus("a11y");
            if (bus.isConnected() && bus.registerObject("/org/aiautodesktop/QtWaylandInput1",
                    this, QDBusConnection::ExportAllSlots)) timer->stop();
        });
        timer->start(250);
    }
public slots:
    QStringList PointerModes() const {
        return {"wheel", "hover", "click", "drag_same_widget", "text_copy_drop", "model_item_copy_drop"};
    }

    bool PointMatches(const QString &target, const QString &window, int px, int py) const {
        auto *widget = boundWidget(target, window);
        return widget && hits(widget, QPoint(px, py));
    }

    int Wheel(const QString &target, const QString &window, int px, int py,
              int direction, int notches, qulonglong deadline) {
        if (delivering || direction < 4 || direction > 7 || notches < 1 || notches > 30) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        auto *widget = boundWidget(target, window);
        const QPoint point(px, py);
        if (!widget || !hits(widget, point) || !inTime(deadline)) return 0;
        // A physical wheel detent is 120 eighths of a degree. Qt uses
        // positive components for up and left in the AT-SPI button mapping.
        const int delta = (direction == 4 || direction == 6 ? 120 : -120) * notches;
        const QPoint angle = direction < 6 ? QPoint(0, delta) : QPoint(delta, 0);
        QWheelEvent event(widget->mapFromGlobal(point), point, {}, angle, Qt::NoButton,
                          Qt::NoModifier, Qt::NoScrollPhase, false, Qt::MouseEventSynthesizedByApplication);
        QCoreApplication::sendEvent(widget, &event);
        return 2;
    }

    int Hover(const QString &target, const QString &window, int px, int py, qulonglong deadline) {
        if (delivering) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        auto *widget = boundWidget(target, window);
        const QPoint point(px, py);
        if (!widget || !hits(widget, point) || !inTime(deadline)) return 0;
        const QPoint local = widget->mapFromGlobal(point);
#if QT_VERSION >= QT_VERSION_CHECK(6, 3, 0)
        QHoverEvent event(QEvent::HoverMove, local, point, local);
#else
        QHoverEvent event(QEvent::HoverMove, local, local);
#endif
        QCoreApplication::sendEvent(widget, &event);
        return 2;
    }

    QByteArray Capabilities(const QStringList &targets, const QStringList &windows) {
        if (targets.size() != windows.size() || targets.size() > 5000) return {};
        QByteArray flags(targets.size(), 0);
        for (int i = 0; i < targets.size(); ++i) {
            if (auto *widget = boundWidget(targets[i], windows[i])) {
                const QRect rect = QAccessible::queryAccessibleInterface(widget)->rect();
                const QPoint center = rect.topLeft() + QPoint(rect.width()/2, rect.height()/2);
                flags[i] = (focused(widget) ? 1 : 0) | (hits(widget, center) ? 2 : 0);
            }
        }
        return flags;
    }

    QByteArray DropRoles(const QStringList &targets, const QStringList &windows) const {
        if (targets.size() != windows.size() || targets.size() > 5000) return {};
        QByteArray roles(targets.size(), 0);
        for (int index = 0; index < targets.size(); ++index) {
            auto *widget = boundWidget(targets[index], windows[index], false);
            if (!widget) {
                auto *item = accessibleFor(targets[index]);
                if (item && item->role() == QAccessible::ListItem &&
                    selectedItemAt(targets[index], windows[index], item->rect().center()).index.isValid())
                    roles[index] = 4;
                continue;
            }
            auto *edit = qobject_cast<QLineEdit *>(widget);
            roles[index] = (edit && edit->dragEnabled() && edit->hasSelectedText() ? 1 : 0) |
                           (widget->acceptDrops() ? 2 : 0);
        }
        return roles;
    }

    // 0 = refused before dispatch, 1 = press began but pair unconfirmed,
    // 2 = both events delivered to the same live object.
    int Key(const QString &target, const QString &window, const QStringList &keys, qulonglong deadline) {
        Chord chord;
        if (delivering || !parseChord(keys, chord)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        if (!widget || !focused(widget) || !inTime(deadline)) return 0;
        QKeyEvent press(QEvent::KeyPress, chord.key, chord.modifiers, chord.text);
        QCoreApplication::sendEvent(widget, &press);
        if (!widget || widget != boundWidget(target, window) || !focused(widget)) return 1;
        QKeyEvent release(QEvent::KeyRelease, chord.key, chord.modifiers, chord.text);
        QCoreApplication::sendEvent(widget, &release);
        return 2;
    }

    int Click(const QString &target, const QString &window, int px, int py, qulonglong deadline) {
        return MouseClick(target, window, px, py, 1, 1, {}, deadline);
    }

    int MouseClick(const QString &target, const QString &window, int px, int py,
                   int button, int count, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        const auto which = mouseButton(button);
        if (delivering || which == Qt::NoButton || (count != 1 && count != 2) ||
            !parseModifiers(modifierNames, modifiers)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        const QPoint point(px, py);
        for (int phase = 0; phase < 2 * count; ++phase) {
            const bool release = phase % 2 != 0;
            // A handler can synchronously move/delete the receiver or open a popup.
            // After a press, release only to the same live widget at the proven hit.
            if (!widget || widget != boundWidget(target, window) || !hits(widget, point) ||
                (!release && !inTime(deadline))) return phase == 0 ? 0 : 1;
            const auto type = release ? QEvent::MouseButtonRelease :
                              phase == 0 ? QEvent::MouseButtonPress : QEvent::MouseButtonDblClick;
            sendMouse(widget, point, type, which, release ? Qt::NoButton : which, modifiers);
        }
        return 2;
    }

    int Drag(const QString &target, const QString &window, int px, int py, int endX, int endY,
             int button, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        const auto which = mouseButton(button);
        if (delivering || which == Qt::NoButton || !parseModifiers(modifierNames, modifiers)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        const QPoint start(px, py), end(endX, endY);
        if (!widget || !lineHits(widget, start, end) || !inTime(deadline)) return 0;
        sendMouse(widget, start, QEvent::MouseButtonPress, which, which, modifiers);
        if (!widget || widget != boundWidget(target, window) ||
            !lineHits(widget, start, end) || !inTime(deadline)) return 1;
        sendMouse(widget, end, QEvent::MouseMove, Qt::NoButton, which, modifiers);
        if (!widget || widget != boundWidget(target, window) || !hits(widget, end)) return 1;
        sendMouse(widget, end, QEvent::MouseButtonRelease, which, Qt::NoButton, modifiers);
        return 2;
    }

    int DropText(const QString &sourceId, const QString &sourceWindow, int startX, int startY,
                 const QString &destId, const QString &destWindow, int endX, int endY,
                 int button, const QStringList &modifierNames, qulonglong deadline) {
        return copyDrop(false, sourceId, sourceWindow, {startX, startY}, destId, destWindow,
                        {endX, endY}, button, modifierNames, deadline);
    }

    int DropModelItem(const QString &sourceId, const QString &sourceWindow, int startX, int startY,
                      const QString &destId, const QString &destWindow, int endX, int endY,
                      int button, const QStringList &modifierNames, qulonglong deadline) {
        return copyDrop(true, sourceId, sourceWindow, {startX, startY}, destId, destWindow,
                        {endX, endY}, button, modifierNames, deadline);
    }
private:
    int copyDrop(bool modelItem, const QString &sourceId, const QString &sourceWindow, QPoint start,
                 const QString &destId, const QString &destWindow, QPoint end,
                 int button, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        if (delivering || button != 1 || !parseModifiers(modifierNames, modifiers) ||
            (modifiers != Qt::NoModifier && modifiers != Qt::ControlModifier)) return 0;
        QScopedValueRollback<bool> busy(delivering, true);
        QPointer<QWidget> dest = boundWidget(destId, destWindow, false);
        QPointer<QWidget> receiver = dropReceiver(dest, end);
        if (!receiver) return 0;
        const QPoint local = receiver->mapFromGlobal(end);
        auto *destList = standardList(dest);
        QPointer<QAbstractItemModel> destModel = destList ? destList->model() : nullptr;
        QPointer<QLineEdit> edit;
        ListItem item;
        QString selection;
        int first = -1;
        bool dataChanged = false;
        QObject watch;
        if (modelItem) {
            item = selectedItemAt(sourceId, sourceWindow, start);
            if (!item.index.isValid() || item.view == dest) return 0;
            connect(item.index.model(), &QAbstractItemModel::dataChanged, &watch,
                    [&dataChanged] { dataChanged = true; });
        } else {
            edit = qobject_cast<QLineEdit *>(boundWidget(sourceId, sourceWindow));
            if (!edit || edit == dest || !edit->hasSelectedText()) return 0;
            first = edit->selectionStart();
            selection = edit->selectedText();
            if (selection.size() > 65536) return 0;
        }
        const auto destinationMatches = [&] {
            return dest && receiver && dest == boundWidget(destId, destWindow, false) &&
                   dropReceiver(dest, end) == receiver && receiver && receiver->mapFromGlobal(end) == local &&
                   (!destList || (dest && destModel && destList->model() == destModel));
        };
        const auto endpointsMatch = [&] {
            if (!destinationMatches() || QApplication::mouseButtons() != Qt::NoButton) return false;
            if (modelItem) {
                const auto current = selectedItemAt(sourceId, sourceWindow, start);
                return !dataChanged && item.index.isValid() && current.index == item.index &&
                       current.view == item.view;
            }
            if (!edit || edit != boundWidget(sourceId, sourceWindow) || !edit->dragEnabled() ||
                !hits(edit, start) || edit->selectionStart() != first || edit->selectedText() != selection)
                return false;
            const int index = edit->cursorPositionAt(edit->mapFromGlobal(start));
            return index >= first && index < first + selection.size();
        };
        if (!endpointsMatch() || !inTime(deadline)) return 0;
        QMimeData payload;
        if (modelItem) {
            if (!copyItemMime(item, payload) || !endpointsMatch() || !inTime(deadline)) return 0;
        } else {
            payload.setText(selection);
        }
        QDragEnterEvent enter(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        if (enter.source()) return 0;
        DropReceiverGuard guard(receiver);
        if (!guard.deliver(receiver, enter)) return 1;
        QDragMoveEvent move(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        if (enter.dropAction() != Qt::CopyAction || !endpointsMatch() || !inTime(deadline) ||
            !guard.deliver(receiver, move) || move.dropAction() != Qt::CopyAction ||
            !endpointsMatch() || !inTime(deadline)) {
            QDragLeaveEvent leave;
            if (receiver) guard.deliver(receiver, leave, destinationMatches());
            return 1;
        }
        QDropEvent drop(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        return guard.deliver(receiver, drop) && drop.dropAction() == Qt::CopyAction ? 2 : 1;
    }
};
}

class InputPlugin : public QGenericPlugin {
    Q_OBJECT
    Q_PLUGIN_METADATA(IID QGenericPluginFactoryInterface_iid FILE "input.json")
public:
    QObject *create(const QString &name, const QString &) override {
        if (name != "aad-wayland-input" || !QGuiApplication::platformName().startsWith("wayland"))
            return nullptr;
        return new Input;
    }
};
#include "input.moc"
