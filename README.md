# Tesla

Tesla — ветка VIctory Scarlett для Windows и Direct3D 12, настроенная для NVIDIA с 8 ГБ VRAM. Основной профиль проверялся на RTX 3060 Ti, Core i5-12400F и 32 ГБ ОЗУ.

По умолчанию используется Series S с выходом **1920×1080** и штатным масштабированием игры. Генерация кадров отключена для замеров обычного FPS. Стабильные 30 FPS на этой конфигурации пока не подтверждены.

## Сборка

Нужны Windows x64, процессор с AVX2, видеокарта с Direct3D 12 и поддержкой mesh shaders, [Rust через rustup](https://rustup.rs/) и Visual Studio Build Tools с компонентами **Desktop development with C++** и Windows SDK. Версия Rust указана в [rust-toolchain.toml](rust-toolchain.toml).

Выполните команду из корня репозитория.

~~~powershell
cargo build --release --locked -p victory
~~~

## Запуск в Full HD

Укажите путь к исполняемому файлу своей копии игры. Папка игры со всеми соседними файлами должна находиться вне репозитория.

~~~powershell
$game = 'D:\Games\MyXboxDump\game_scarlett_master.exe'
.\scripts\play-3070.ps1 -Game $game -FullHD
~~~

Если политика PowerShell блокирует локальный скрипт, укажите разрешение на один процесс.

~~~powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\play-3070.ps1 -Game $game -FullHD
~~~

Логи находятся в `runs\`. Кэш по умолчанию хранится в `%LOCALAPPDATA%\victory\cache`, сохранения — в `%LOCALAPPDATA%\victory\data`. Чтобы вынести кэш на другой диск, задайте `VICTORY_CACHE_DIR` перед запуском. Пути также можно настроить через [victory.example.toml](victory.example.toml), скопировав его в `victory.toml`.

## Шейдеры и конвейеры

**Шейдеры** — программы GPU, которые VIctory переводит для ПК. **Конвейер** — сочетание шейдеров, форматов и состояния рендера, которое собирает драйвер. Оба результата можно подготовить заранее.

1. **Первый прогрев.** Лаунчер переведёт шейдеры, прогреет известные конвейеры и запустит игру. Это может занять несколько минут.

   ~~~powershell
   .\scripts\play-3070.ps1 -Game $game -FullHD -Prewarm
   ~~~

2. **Запишите конвейеры нужной сцены.** Дойдите до катсцены или места со статтерами, затем штатно закройте игру. Лаунчер сам включает запись запрошенных конвейеров. В пустом кэше списку ранее запрошенных конвейеров пока не из чего собираться.

3. **Прогрейте записанное.** Лаунчер использует один и тот же исполняемый файл, кэш и настройки для прогрева и игры.

   ~~~powershell
   .\scripts\play-3070.ps1 -Game $game -FullHD -PrepareCutscenes
   ~~~

   Для прогрева с выходом сразу после окончания используйте этот режим.

   ~~~powershell
   .\scripts\play-3070.ps1 -Game $game -FullHD -WarmOnly
   ~~~

Сочетание `-Prewarm -WarmOnly` сначала переводит шейдеры, затем прогревает ранее запрошенные конвейеры. Прогрев полезно повторить после смены драйвера, сборки или параметров перевода. Обычный запуск переиспользует существующий кэш.

При ручной настройке окружения используйте команды CLI напрямую.

~~~powershell
.\target\release\victory.exe shaders (Split-Path -Parent $game)
.\target\release\victory.exe warm --used
~~~

Сохраните тот же путь к кэшу и те же параметры перевода, что использует игра. `warm` без `--used` перебирает все известные записи. Список опций прогрева доступен через `warm --help`.

## Настройки

| Параметр | Поведение |
| --- | --- |
| `-FullHD` | Series S, выход 1920×1080. Это также профиль по умолчанию. |
| `-Console series-x` | Профиль Series X. Используйте отдельно от `-FullHD`. |
| `-MipDrop 1` | Стандартный профиль текстур для 8 ГБ. Значение `0` сохраняет исходные mip-уровни и требует больше VRAM. |
| `-Dlss Quality` | Экспериментальное масштабирование в Series S. Нужен NVIDIA Streamline. |
| `-OriginalSceneLoading` | Отключает повторную проверку текстур после переполнения очереди обновлений. |
| `-OriginalScheduler` | Возвращает прежние приоритеты потоков и порядок записи кэша конвейеров. |
| `-SendSpareMB 768` | Лимит запаса свободных буферов отправки в МиБ. |
| `-JoinLists 48` | Лимит списков команд в одной группе подготовки. |

Для DLSS распакуйте официальный NVIDIA Streamline SDK 2.14.16 вне репозитория. [Скачать Streamline SDK](https://github.com/NVIDIA-RTX/Streamline/releases/tag/v2.14.16)

~~~powershell
$env:VICTORY_STREAMLINE_DIR = 'D:\SDK\Streamline\bin\x64'
.\scripts\play-3070.ps1 -Game $game -FullHD -Dlss Quality
~~~

В тесте DLSS Quality работал на RTX 3060 Ti, но в начале катсцены были длинные зависания. Для сравнения FPS оставляйте один и тот же метод масштабирования, сцену и прогретый кэш.

## Изменения в Tesla

- При переполнении очереди обновлений повторно проверяются размещённые текстуры.
- Исправлено декодирование цветовых каналов BC7. Освобождение слотов дескрипторов учитывает, какая GPU-таблица их выделила.
- Повышен приоритет потоков, которых ждёт отправка команд. Библиотека конвейеров записывается в фоне.
- При обратном копировании переиспользуются временные буферы. Порядок чтений и записей сохранён.

Сравнивайте FPS и время кадра в одной и той же 3D-сцене. Для диагностики смотрите также время копирования и число отрисовок, пропущенных из-за неготовых конвейеров.

## Авторство и лицензия

База проекта — [Yaekai/victory-scarlett-3070](https://github.com/Yaekai/victory-scarlett-3070/tree/scarlett-fox), [Op-lite/victory-scarlett](https://github.com/Op-lite/victory-scarlett) и [Marvisak/VIctory](https://github.com/Marvisak/VIctory). В адаптации использованы правки [VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA), [Foxhound](https://github.com/readmet3xt-lab/foxhound), [KLBF Lab](https://github.com/PastiiJay/klbf-lab) и [Soldier16](https://github.com/soldier162/Victory-nvidia-soldier16). Подробности и авторы исходных изменений сохранены в истории и комментариях.

Лицензия [GPL-3.0-or-later](LICENSE). Правила размещения исходников и исключение для таблиц формата BCPack описаны в [политике материалов](docs/asset-policy.md). Данные игры и библиотеки NVIDIA предоставляет пользователь.
