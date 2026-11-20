#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shlobj.h>
#include <propkey.h>
#include <propsys.h>
#include <propvarutil.h>
#include <wrl.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Data.Json.h>
#include <winrt/Windows.Data.Xml.Dom.h>
#include <winrt/Windows.UI.Notifications.h>
#include <iostream>
#include <string>
#include <stdexcept>
#include <io.h>
#include <fcntl.h>

using Microsoft::WRL::ComPtr;
static constexpr wchar_t Identity[] = L"dev.zcalld.desktop";
static std::wstring executable() {
    std::wstring path(32768, L'\0');
    DWORD n = GetModuleFileNameW(nullptr, path.data(), static_cast<DWORD>(path.size()));
    if (n == 0 || n >= path.size()) throw std::runtime_error("identity");
    path.resize(n); return path;
}
static std::wstring shortcut() {
    PWSTR value = nullptr;
    winrt::check_hresult(SHGetKnownFolderPath(FOLDERID_Programs, 0, nullptr, &value));
    std::wstring path(value); CoTaskMemFree(value);
    return path + L"\\zcalld Desktop.lnk";
}
static ComPtr<IShellLinkW> shellLink() {
    ComPtr<IShellLinkW> link;
    winrt::check_hresult(CoCreateInstance(CLSID_ShellLink, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&link)));
    return link;
}
static bool registered(const std::wstring &path) {
    auto link = shellLink(); ComPtr<IPersistFile> file; winrt::check_hresult(link.As(&file));
    if (FAILED(file->Load(shortcut().c_str(), STGM_READ))) return false;
    wchar_t target[32768] = {};
    if (FAILED(link->GetPath(target, 32768, nullptr, SLGP_RAWPATH)) || _wcsicmp(target, path.c_str()) != 0) return false;
    ComPtr<IPropertyStore> props; winrt::check_hresult(link.As(&props));
    PROPVARIANT value; PropVariantInit(&value);
    HRESULT hr = props->GetValue(PKEY_AppUserModel_ID, &value);
    bool good = SUCCEEDED(hr) && value.vt == VT_LPWSTR && value.pwszVal && wcscmp(value.pwszVal, Identity) == 0;
    PropVariantClear(&value); return good;
}
static void registerIdentity(const std::wstring &path) {
    auto link = shellLink(); winrt::check_hresult(link->SetPath(path.c_str()));
    winrt::check_hresult(link->SetArguments(L"--probe"));
    ComPtr<IPropertyStore> props; winrt::check_hresult(link.As(&props));
    PROPVARIANT value; winrt::check_hresult(InitPropVariantFromString(Identity, &value));
    HRESULT hr = props->SetValue(PKEY_AppUserModel_ID, value); PropVariantClear(&value); winrt::check_hresult(hr);
    winrt::check_hresult(props->Commit()); ComPtr<IPersistFile> file; winrt::check_hresult(link.As(&file));
    winrt::check_hresult(file->Save(shortcut().c_str(), TRUE));
}
static winrt::Windows::Data::Xml::Dom::XmlDocument document(const std::string &input) {
    if (input.size() > 2048) throw std::runtime_error("input");
    auto value = winrt::Windows::Data::Json::JsonObject::Parse(winrt::to_hstring(input));
    auto title = value.GetNamedString(L"title"), body = value.GetNamedString(L"body");
    if (title.size() > 160 || body.size() > 1600) throw std::runtime_error("input");
    winrt::Windows::Data::Xml::Dom::XmlDocument doc;
    doc.LoadXml(L"<toast><visual><binding template='ToastGeneric'><text/><text/></binding></visual></toast>");
    auto texts = doc.GetElementsByTagName(L"text");
    texts.Item(0).InnerText(title); texts.Item(1).InnerText(body); // Text, never XML markup.
    return doc;
}
int wmain(int argc, wchar_t *argv[]) {
    try {
        if (argc != 2) return 5;
        _setmode(_fileno(stdout), _O_BINARY);
        winrt::init_apartment(winrt::apartment_type::single_threaded);
        std::wstring mode(argv[1]);
        if (mode == L"--self-test") {
            auto doc = document("{\"title\":\"Fixture <tag>\",\"body\":\"Synthetic\"}");
            if (doc.GetElementsByTagName(L"text").Item(0).InnerText() != L"Fixture <tag>") return 5;
            std::cout << "self-test passed\n"; return 0;
        }
        if (mode != L"--notify-stdin" && mode != L"--probe" && mode != L"--register") return 5;
        const auto path = executable();
        if (mode == L"--register") { registerIdentity(path); std::cout << "registered\n"; return 0; }
        if (!registered(path)) return 4;
        winrt::check_hresult(SetCurrentProcessExplicitAppUserModelID(Identity));
        auto notifier = winrt::Windows::UI::Notifications::ToastNotificationManager::CreateToastNotifier(Identity);
        if (notifier.Setting() != winrt::Windows::UI::Notifications::NotificationSetting::Enabled) return 3;
        if (mode == L"--probe") { std::cout << "available\n"; return 0; }
        std::string input; char c;
        while (std::cin.get(c)) { if (input.size() == 2048) return 5; input.push_back(c); }
        winrt::Windows::UI::Notifications::ToastNotification toast(document(input));
        // No activation handler, browser URL or account/credential payload.
        notifier.Show(toast); std::cout << "accepted\n"; return 0;
    } catch (...) { return 5; } // Never print arbitrary OS error text or stdin.
}
