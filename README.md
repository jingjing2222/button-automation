# Button Automation

Tauri 기반 버튼 검사/자동 클릭 앱입니다. 앱을 켜면 Google 대상 웹뷰가 자동으로 뜨고, 인스펙터로 버튼을 선택한 뒤 지정한 주기마다 같은 버튼을 찾아 클릭합니다.

## 사용

```bash
npm install
npm run tauri:dev
```

1. 앱 실행 후 Google 대상 웹뷰가 뜨는지 확인합니다.
2. `인스펙터`를 누른 뒤 대상 웹뷰에서 원하는 버튼 위에 마우스를 올립니다.
3. 초록색 레이아웃이 표시된 상태에서 클릭하면 버튼이 선택됩니다.
4. 주기를 조정하고 `시작`을 누르면 선택한 버튼을 반복해서 찾고 클릭합니다.

컨트롤 창 왼쪽에는 주기적으로 동기화한 대상 화면/DOM 위치가 표시되고, 오른쪽 로그에는 `찾는 중`, `찾았다`, `눌렀다`, `비활성화` 상태가 누적됩니다.

## 빌드

```bash
npm run build:mac
npm run build:windows
```

macOS 로컬 빌드 산출물은 `src-tauri/target/release/bundle/` 아래에 생성됩니다. Windows 빌드는 Windows 러너가 필요하므로 `.github/workflows/build.yml`의 `Build desktop apps` 워크플로에서 MSI/NSIS 산출물을 만들도록 구성했습니다.
