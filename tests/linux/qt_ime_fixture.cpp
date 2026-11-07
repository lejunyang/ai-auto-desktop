#include <QApplication>
#include <QAccessible>
#include <QDir>
#include <QInputMethodEvent>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QSaveFile>
#include <QScreen>
#include <QTextStream>
#include <QTimer>
#include <QVBoxLayout>

// Only this isolated application's synthetic input is recorded. The native
// backend never supplies these observations; Qt's IME events do.
class ImeEntry : public QLineEdit {
public:
    ImeEntry(QWidget *parent, QString output) : QLineEdit(parent), output_(std::move(output)) {
        setAccessibleName("IME test entry");
        connect(this, &QLineEdit::textChanged, this, [this] { persist(); });
        persist();
    }
protected:
    void inputMethodEvent(QInputMethodEvent *event) override {
        preedit_ = event->preeditString();
        seen_preedit_ = seen_preedit_ || !preedit_.isEmpty();
        if (!event->commitString().isEmpty()) {
            ++commits_;
            last_commit_ = event->commitString();
        }
        QLineEdit::inputMethodEvent(event);
        persist();
        if (!preedit_.isEmpty()) {
            QTimer::singleShot(250, this, [this] {
                if (preedit_.isEmpty() || QApplication::activeWindow() != window()) return;
                // Crop to this known foreground fixture, never the full desktop.
                const auto origin = window()->mapToGlobal(QPoint(0, 0));
                QScreen *screen = window()->screen();
                if (screen) screen->grabWindow(0, origin.x(), origin.y(), window()->width(), window()->height())
                    .save(output_ + "/candidate.png");
            });
        }
    }
private:
    void persist() {
        QSaveFile file(output_ + "/fixture-state.json");
        if (!file.open(QIODevice::WriteOnly)) return;
        const QJsonObject state{{"preedit_seen", seen_preedit_}, {"preedit", preedit_},
                                {"commits", commits_}, {"last_commit", last_commit_}, {"text", text()}};
        file.write(QJsonDocument(state).toJson(QJsonDocument::Compact));
        file.commit();
    }
    QString output_, preedit_, last_commit_;
    bool seen_preedit_ = false;
    int commits_ = 0;
};

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    QAccessible::setActive(true);
    app.setApplicationName("AAD Qt IME Fixture");
    const QString output = qEnvironmentVariable("AAD_IME_TEST_OUTPUT");
    if (output.isEmpty() || !QDir().mkpath(output)) return 2;
    QWidget window;
    window.setWindowTitle("AAD Qt IME Fixture");
    window.setAccessibleName("AAD Qt IME Fixture");
    window.resize(700, 300);
    auto *layout = new QVBoxLayout(&window);
    layout->addWidget(new QLabel("Isolated physical-key IME verification", &window));
    auto *entry = new ImeEntry(&window, output);
    layout->addWidget(entry);
    layout->addStretch();
    window.show(); window.raise(); window.activateWindow(); entry->setFocus();
    QTextStream(stdout) << "READY\n" << Qt::flush;
    return app.exec();
}
