本目录是构建期资源目录（本次纯源码交付不含内容）。

tauri.conf.json 的 bundle.resources 引用 resources/web/，
由 packaging/build_all.bat 在打包时从项目根 web/ 拷贝生成。
源码运行时可手工执行： xcopy /E /I /Y web shell\tauri\src-tauri\resources\web
