#include "qt_shim.h"

#include <QAction>
#include <QActionGroup>
#include <QApplication>
#include <QCoreApplication>
#include <QCursor>
#include <QDir>
#include <QElapsedTimer>
#include <QFont>
#include <QFontDatabase>
#include <QGuiApplication>
#include <QIcon>
#include <QLocalServer>
#include <QLocalSocket>
#include <QLockFile>
#include <QMenu>
#include <QPointer>
#include <QQuickStyle>
#include <QScreen>
#include <QStandardPaths>
#include <QSystemTrayIcon>
#include <QSysInfo>
#include <QThread>
#include <QTimer>
#include <QWidget>

#ifdef Q_OS_WIN
#include <QCryptographicHash>
#include <qt_windows.h>
#endif

#include <vector>

void lecooSetApplicationIcon()
{
    QGuiApplication::setWindowIcon(QIcon(QStringLiteral(":/assets/app-icon.png")));
}

void lecooSetDesktopFileName(const QString &name)
{
    QGuiApplication::setDesktopFileName(name);
}

void lecooSetQuickControlsStyle(const QString &style)
{
    QQuickStyle::setStyle(style);
}

QString lecooSystemFontFamily()
{
    return QFontDatabase::systemFont(QFontDatabase::GeneralFont).family();
}

QString lecooFixedFontFamily()
{
    return QFontDatabase::systemFont(QFontDatabase::FixedFont).family();
}

QString lecooCursorScreenName()
{
    // Resolve the cursor screen across mixed-DPI coordinate systems.
    const QPoint pos = QCursor::pos();
    const QScreen *direct = QGuiApplication::screenAt(pos);
    if (direct)
        return direct->name();

    const QList<QScreen *> screens = QGuiApplication::screens();
    for (const QScreen *screen : screens) {
        const QRect logical = screen->geometry();
        const qreal ratio = screen->devicePixelRatio();
        const QPoint mapped = logical.topLeft()
                + QPoint(qRound((pos.x() - logical.x()) / ratio),
                         qRound((pos.y() - logical.y()) / ratio));
        if (logical.contains(mapped))
            return screen->name();
    }
    return QString();
}

QString lecooPlatformName()
{
    return QGuiApplication::platformName();
}

QString lecooOsName()
{
    // e.g. "Windows 11", "CachyOS Linux"
    const QString pretty = QSysInfo::prettyProductName();
    if (!pretty.isEmpty())
        return pretty;

    const QString type = QSysInfo::productType();
    if (!type.isEmpty())
        return type.front().toUpper() + type.mid(1);

    return QStringLiteral("Unknown OS");
}

QString lecooOsVersion()
{
    // e.g. "10.0.22631 · x86_64" on Windows, "6.12.4 · x86_64" on Linux
    const QString kernel = QSysInfo::kernelVersion();
    const QString arch = QSysInfo::currentCpuArchitecture();
    if (kernel.isEmpty())
        return arch;
    return kernel + QStringLiteral(" · ") + arch;
}

std::unique_ptr<QGuiApplication> lecooNewApplication(const QStringList &args)
{
    // Qt retains argc/argv, so their storage must outlive the application.
    static std::vector<QByteArray> bytes;
    static std::vector<char *> argv;
    static int argc;
    for (const auto &arg : args)
        bytes.push_back(arg.toLocal8Bit());
    for (auto &arg : bytes)
        argv.push_back(arg.data());
    argc = static_cast<int>(argv.size());
    argv.push_back(nullptr);
    return std::make_unique<QApplication>(argc, argv.data());
}

namespace {
QPointer<QObject> activationTarget;

struct SingleInstance : QObject
{
    explicit SingleInstance(const QString &path) : lock(path) { lock.setStaleLockTime(0); }

    QLockFile lock;
    QLocalServer server;
};

constexpr int maxActivationMessage = 4096;

void receiveActivation(QLocalSocket *socket)
{
    socket->setReadBufferSize(maxActivationMessage);
    QObject::connect(socket, &QLocalSocket::disconnected, socket, &QObject::deleteLater);
    QTimer::singleShot(3000, socket, &QObject::deleteLater);
    const auto receive = [socket] {
        if (!socket->canReadLine()) {
            if (socket->bytesAvailable() >= maxActivationMessage)
                socket->abort();
            return;
        }
        auto token = socket->readLine();
        token.chop(1);
        if (activationTarget) {
            // Qt Wayland consumes this token when the existing window requests activation.
            if (!token.isEmpty() && QGuiApplication::platformName().startsWith(QStringLiteral("wayland")))
                qputenv("XDG_ACTIVATION_TOKEN", token);
            QMetaObject::invokeMethod(activationTarget.data(), "handleTrayAction", Q_ARG(int, 0));
        }
        socket->disconnectFromServer();
    };
    QObject::connect(socket, &QLocalSocket::readyRead, socket, receive);
    receive();
}

// No custom meta-object or Rust callback ownership: Qt owns all tray objects.
class LecooTray : public QObject
{
public:
    explicit LecooTray(QObject &controller) : QObject(&controller), menu(new QMenu),
        icon(new QSystemTrayIcon(QIcon(QStringLiteral(":/assets/app-icon.png")), this))
    {
        icon->setToolTip(QStringLiteral("Lecoo Control Panel QML"));
        icon->setContextMenu(menu.get());
        auto *group = new QActionGroup(this);
        group->setExclusive(true);
        for (int id = 0; id < 5; ++id) {
            if (id == 1 || id == 4)
                menu->addSeparator();
            auto *action = menu->addAction(QString());
            actions.append(action);
            if (id >= 1 && id <= 3) {
                action->setCheckable(true);
                group->addAction(action);
            }
            QObject::connect(action, &QAction::triggered, &controller, [&controller, id] {
                QMetaObject::invokeMethod(&controller, "handleTrayAction", Q_ARG(int, id));
            });
        }
        QObject::connect(icon, &QSystemTrayIcon::activated, &controller,
                         [&controller](QSystemTrayIcon::ActivationReason reason) {
            if (reason == QSystemTrayIcon::Trigger || reason == QSystemTrayIcon::DoubleClick)
                QMetaObject::invokeMethod(&controller, "handleTrayAction", Q_ARG(int, 0));
        });
        icon->show();
    }
    ~LecooTray() override { icon->hide(); icon->setContextMenu(nullptr); }
    std::unique_ptr<QMenu> menu;
    QSystemTrayIcon *icon;
    QList<QAction *> actions;
};
QPointer<LecooTray> tray;
}

int lecooStartSingleInstance()
{
#ifdef Q_OS_WIN
    const auto directory = QStandardPaths::writableLocation(QStandardPaths::AppLocalDataLocation);
#else
    const auto directory = QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation);
#endif
    if (directory.isEmpty() || !QDir().mkpath(directory))
        return 2;
    const auto path = QDir(directory).filePath(QCoreApplication::applicationName() + QStringLiteral(".lock"));
#ifdef Q_OS_WIN
    const auto name = QStringLiteral("lecoo-")
        + QString::fromLatin1(QCryptographicHash::hash(path.toUtf8(), QCryptographicHash::Sha256).toHex().left(32));
#else
    const auto name = QDir(directory).filePath(QCoreApplication::applicationName());
#endif
    auto instance = std::make_unique<SingleInstance>(path);
    QElapsedTimer startup;
    startup.start();
    do {
        if (instance->lock.tryLock(0)) {
            // Only the lock owner may clean up a socket left by a crashed process.
            QLocalServer::removeServer(name);
            instance->server.setSocketOptions(QLocalServer::UserAccessOption);
            if (!instance->server.listen(name))
                return 2;
            auto *server = &instance->server;
            QObject::connect(server, &QLocalServer::newConnection, server, [server] {
                while (auto *socket = server->nextPendingConnection())
                    receiveActivation(socket);
            });
            instance->setParent(qApp);
            instance.release();
            return 0;
        }
        if (instance->lock.error() != QLockFile::LockFailedError)
            return 2;

        QLocalSocket socket;
        socket.connectToServer(name, QIODevice::WriteOnly);
        if (socket.waitForConnected(100)) {
#ifdef Q_OS_WIN
            qint64 pid;
            QString host, app;
            if (instance->lock.getLockInfo(&pid, &host, &app))
                AllowSetForegroundWindow(static_cast<DWORD>(pid));
#endif
            auto token = qgetenv("XDG_ACTIVATION_TOKEN");
            if (token.size() >= maxActivationMessage || token.contains('\n') || token.contains('\0'))
                token.clear();
            token.append('\n');
            if (socket.write(token) != token.size())
                return 2;
            while (socket.bytesToWrite() > 0) {
                if (!socket.waitForBytesWritten(1000))
                    return 2;
            }
            return 1;
        }
        QThread::msleep(50);
    } while (startup.elapsed() < 3000);
    return 2;
}

bool lecooTrayAvailable()
{
    return QSystemTrayIcon::isSystemTrayAvailable();
}

void lecooInitTray(QObject &controller)
{
    activationTarget = &controller;
    if (!tray)
        tray = new LecooTray(controller);
}

void lecooTraySetMenu(const QStringList &items, int checkedIndex, bool enabled)
{
    if (!tray || items.size() != 5)
        return;
    for (int id = 0; id < 5; ++id) {
        auto *action = tray->actions[id];
        action->setText(items[id]);
        if (id >= 1 && id <= 3) {
            action->setChecked(id - 1 == checkedIndex);
            action->setEnabled(enabled);
        }
    }
}
