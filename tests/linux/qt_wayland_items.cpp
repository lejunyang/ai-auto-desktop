#include <QApplication>
#include <QDragEnterEvent>
#include <QLabel>
#include <QListWidget>
#include <QMimeData>
#include <QPushButton>
#include <QStandardItemModel>
#include <QTextStream>
#include <QVBoxLayout>
#include <functional>

namespace {
void receipt(const QString &value) {
    QTextStream(stdout) << value << '\n' << Qt::flush;
}

const QString invoiceFormat = "application/x-invoice-id";
const QByteArray invoiceId("A\0B\0C", 5);

class InvoiceModel : public QStandardItemModel {
public:
    bool moveOnly = false;
    Qt::DropActions supportedDragActions() const override {
        return moveOnly ? Qt::MoveAction : Qt::CopyAction;
    }
    QStringList mimeTypes() const override { return {invoiceFormat}; }
    QMimeData *mimeData(const QModelIndexList &indexes) const override {
        if (indexes.size() != 1) return nullptr;
        auto *mime = new QMimeData;
        mime->setData(invoiceFormat, indexes.first().data(Qt::UserRole).toByteArray());
        receipt("MODEL MIME " + indexes.first().data().toString());
        return mime;
    }
};

class InvoiceReceiver : public QLabel {
public:
    std::function<void()> onEnter;
    explicit InvoiceReceiver(const QString &name) : QLabel(name) {
        setAccessibleName(name);
        setAcceptDrops(true);
    }
protected:
    bool accepts(const QDropEvent *event) const {
        return event->possibleActions() == Qt::CopyAction && event->proposedAction() == Qt::CopyAction &&
               event->mimeData()->formats() == QStringList{invoiceFormat} &&
               event->mimeData()->data(invoiceFormat) == invoiceId;
    }
    void dragEnterEvent(QDragEnterEvent *event) override {
        if (!accepts(event)) return;
        event->acceptProposedAction();
        if (onEnter) onEnter();
    }
    void dragMoveEvent(QDragMoveEvent *event) override {
        if (accepts(event)) event->acceptProposedAction();
    }
    void dropEvent(QDropEvent *event) override {
        if (!accepts(event) || event->dropAction() != Qt::CopyAction) return;
        receipt("BINARY COPY " + accessibleName());
        event->acceptProposedAction();
    }
};
}

int itemFixture(QApplication &app) {
    QWidget other;
    other.setWindowTitle("Remote invoice list");
    QVBoxLayout otherLayout(&other);
    QListWidget remote;
    remote.setAccessibleName("Remote basket");
    remote.setDragDropMode(QAbstractItemView::DropOnly);
    otherLayout.addWidget(&remote);
    other.show();

    QWidget window;
    window.setWindowTitle("Invoice lists");
    QVBoxLayout layout(&window);
    QListWidget source;
    source.setDragDropMode(QAbstractItemView::DragOnly);
    source.setSelectionMode(QAbstractItemView::ExtendedSelection);
    source.addItems({"Invoice 2037", "Invoice 2042"});
    source.item(0)->setData(Qt::UserRole, "line-id-19");
    source.setCurrentRow(0);
    layout.addWidget(&source);
    QListWidget local;
    local.setAccessibleName("Local basket");
    local.setDragDropMode(QAbstractItemView::DropOnly);
    layout.addWidget(&local);

    InvoiceModel model;
    auto *invoice = new QStandardItem("Binary invoice");
    invoice->setData(invoiceId, Qt::UserRole);
    model.appendRow(invoice);
    QListView view;
    view.setDragDropMode(QAbstractItemView::DragOnly);
    view.setModel(&model);
    view.setCurrentIndex(invoice->index());
    layout.addWidget(&view);
    for (auto *list : {static_cast<QListView *>(&source), static_cast<QListView *>(&local), &view})
        list->setFixedHeight(76);
    InvoiceReceiver custom("Binary receiver");
    layout.addWidget(&custom);
    InvoiceReceiver changeSelection("Selection-changing receiver");
    changeSelection.onEnter = [&] {
        view.clearSelection();
        receipt("CLEARED DURING ENTER");
    };
    layout.addWidget(&changeSelection);
    QPushButton multiple("Select both invoices");
    QObject::connect(&multiple, &QPushButton::clicked, [&] {
        source.item(1)->setSelected(true);
        receipt(QString("SELECTED %1").arg(source.selectedItems().size()));
    });
    layout.addWidget(&multiple);
    QPushButton moveOnly("Allow moves only");
    QObject::connect(&moveOnly, &QPushButton::clicked, [&] {
        view.setCurrentIndex(invoice->index());
        view.selectionModel()->select(invoice->index(), QItemSelectionModel::ClearAndSelect);
        model.moveOnly = true;
        receipt("MOVE ONLY");
    });
    layout.addWidget(&moveOnly);
    QPushButton audit("Check and empty baskets");
    QObject::connect(&audit, &QPushButton::clicked, [&] {
        for (auto *list : {&local, &remote}) {
            if (list->count() == 1)
                receipt(list->accessibleName() + " " + list->item(0)->text() + " " +
                        list->item(0)->data(Qt::UserRole).toString());
            else receipt(list->accessibleName() + QString(" count %1").arg(list->count()));
            list->clear();
        }
        if (source.count() == 2 && source.item(0)->isSelected() && source.item(0)->text() == "Invoice 2037" &&
            source.item(0)->data(Qt::UserRole) == "line-id-19" && model.rowCount() == 1 &&
            invoice->data(Qt::UserRole) == invoiceId) receipt("SOURCE INTACT");
    });
    layout.addWidget(&audit);
    window.show();
    return app.exec();
}
