# Pincer Qt Probe (PincerTestHarness.Qt6). Build out of tree, for example
#   mkdir -p rust/test-apps/harness-qt6 && cd rust/test-apps/harness-qt6
#   qmake6 ../../../tests/fixtures/apps/linux/qt6/qt6.pro && make
# or run tests/fixtures/build/linux.sh --only qt6.
TEMPLATE = app
TARGET = PincerTestHarness.Qt6
QT += widgets
CONFIG += c++17 warn_on
SOURCES += main.cpp
