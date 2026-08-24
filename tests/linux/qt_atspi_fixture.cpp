#include <QApplication>
#include <QAccessible>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QTextStream>
#include <QVBoxLayout>
#include <QWidget>

namespace {
constexpr auto kApplicationName = "AAD Qt AT-SPI Fixture";
constexpr auto kEntryName = "Qt fixture text entry";
constexpr auto kTypeEntryName = "Qt fixture XTest text entry";
constexpr auto kButtonName = "Invoke Qt fixture button";
constexpr auto kStatusInitial = "Qt fixture status idle";
constexpr auto kStatusInvoked = "Qt fixture status invoked";
}

int main(int argc, char **argv) {
    QApplication application(argc, argv);
    QAccessible::setActive(true);
    application.setApplicationName(kApplicationName);
    application.setOrganizationName("ai-auto-desktop");

    QWidget window;
    window.setWindowTitle(kApplicationName);
    window.setAccessibleName(kApplicationName);
    window.setObjectName("qt-fixture-window");
    window.resize(420, 180);

    auto *layout = new QVBoxLayout(&window);

    auto *entry = new QLineEdit(&window);
    entry->setText("Qt fixture initial text");
    entry->setAccessibleName(kEntryName);
    entry->setObjectName("qt-fixture-entry");
    layout->addWidget(entry);

    auto *typeEntry = new QLineEdit(&window);
    typeEntry->setText("");
    typeEntry->setAccessibleName(kTypeEntryName);
    typeEntry->setObjectName("qt-fixture-xtest-entry");
    layout->addWidget(typeEntry);

    auto *button = new QPushButton("Invoke", &window);
    button->setAccessibleName(kButtonName);
    button->setObjectName("qt-fixture-button");
    layout->addWidget(button);

    auto *status = new QLabel(kStatusInitial, &window);
    status->setAccessibleName(kStatusInitial);
    status->setObjectName("qt-fixture-status");
    layout->addWidget(status);

    QObject::connect(button, &QPushButton::clicked, status, [status]() {
        status->setText(kStatusInvoked);
        status->setAccessibleName(kStatusInvoked);
    });

    window.show();
    window.raise();
    window.activateWindow();
    typeEntry->setFocus(Qt::OtherFocusReason);
    QTextStream(stdout) << "READY\n" << Qt::flush;
    return application.exec();
}
