#include <QApplication>
#include <QAccessible>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QTextStream>
#include <QVBoxLayout>
#include <QWidget>

// The test controller provides its expected synthetic secret in the environment.
// Only equality/empty status is exposed; no secret or length is logged or shown.
int main(int argc, char **argv) {
    QApplication application(argc, argv);
    QAccessible::setActive(true);
    application.setApplicationName("AAD Qt Secret Fixture");
    application.setOrganizationName("ai-auto-desktop");
    const QString expected = qEnvironmentVariable("AAD_SECRET_FIXTURE_EXPECTED");

    QWidget window;
    window.setWindowTitle("AAD Qt Secret Fixture");
    window.setAccessibleName("AAD Qt Secret Fixture");
    window.resize(400, 180);
    auto *layout = new QVBoxLayout(&window);
    auto *entry = new QLineEdit(&window);
    entry->setEchoMode(QLineEdit::Password);
    entry->setText("replace-this-placeholder");
    entry->setAccessibleName("Fixture protected entry");
    entry->setObjectName("fixture-protected-entry");
    layout->addWidget(entry);

    auto *status = new QLabel("Secret status idle", &window);
    status->setAccessibleName("Secret status idle");
    auto *check = new QPushButton("Check replacement", &window);
    check->setAccessibleName("Check secret replacement");
    layout->addWidget(check);
    QObject::connect(check, &QPushButton::clicked, status, [entry, status, expected]() {
        const QString result = entry->text() == expected ? "Secret replacement matched" : "Secret replacement mismatch";
        status->setText(result);
        status->setAccessibleName(result);
    });
    auto *clear = new QPushButton("Check empty", &window);
    clear->setAccessibleName("Check secret cleared");
    layout->addWidget(clear);
    QObject::connect(clear, &QPushButton::clicked, status, [entry, status]() {
        const QString result = entry->text().isEmpty() ? "Secret cleared" : "Secret not cleared";
        status->setText(result);
        status->setAccessibleName(result);
    });
    layout->addWidget(status);
    window.show();
    window.raise();
    window.activateWindow();
    QTextStream(stdout) << "READY\n" << Qt::flush;
    return application.exec();
}
