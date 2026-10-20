"""Stress scenarios for measuring `ebira ask`: made-up logs that each strain one step.

tabi-app (Claude Code): short agreements whose meaning is in the agent's words around them,
and a long pasted spec with what matters in its middle.
atelier (Claude Code): hundreds of the person's design remarks, and three rules restated many
times in other words.
sensor-hub (Codex): causes an agent found months ago, under many newer agent records holding
the same words.

The same sessions every time: run.py writes them with the scenarios.
"""

import datetime
import random

TABI = ("tabi-app", "/work/tabi-app")
ATELIER = ("atelier", "/work/atelier")
SENSOR = ("sensor-hub", "/work/sensor-hub")


def new_session(app, place, date, label):
    project, cwd = place
    return {"app": app, "project": project, "cwd": cwd, "date": date,
            "label": f"{project}/{label}", "records": []}


class Writer:
    """Appends records to a session, each a minute or more after the one before."""

    def __init__(self, session, start):
        hour, minute = map(int, start.split(":"))
        self.session = session
        self.now = hour * 60 + minute

    def add(self, kind, text, key=None, tool=None, output=None, step=1):
        self.now += step
        if self.now >= 23 * 60 + 50:
            raise SystemExit(f"{self.session['label']}: a session runs past midnight")
        self.session["records"].append({
            "time": f"{self.now // 60:02d}:{self.now % 60:02d}", "kind": kind, "text": text,
            "output": list(output or []), "key": key, "tool": tool})

    def you(self, text, key=None, step=2):
        self.add("you", text, key, step=step)

    def agent(self, text, key=None, step=1):
        self.add("agent", text, key, step=step)

    def tool(self, name, text, output, step=1):
        self.add("tool", text, None, tool=name, output=output.splitlines() or ["(no output)"], step=step)


def dates(rng, first, last, count):
    """`count` dates between two (month, day) pairs of 2026, sorted, repeats allowed."""
    start = datetime.date(2026, *first)
    span = (datetime.date(2026, *last) - start).days
    return sorted((start + datetime.timedelta(days=rng.randint(0, span))).isoformat()
                  for _ in range(count))


# ---------------------------------------------------------------------------------------
# tabi-app: short agreements, and a long spec

ACK_PAIRS = [
    ("ボタンの角丸を 8px と 12px で比べました。12px の方が柔らかい印象なので 12px をおすすめします。", "それでいこう", "角丸を 12px にしました。"),
    ("日付は「7月8日(火)」の形で出すのが読みやすいと思います。", "いいね、そっちで", "日付をその形式にしました。"),
    ("読み込みに失敗したときは、再試行ボタンを出す案がいいと思います。", "OK それ採用", "再試行ボタンを出すようにしました。"),
    ("旅程名が長いときは2行まで出して、残りは省略する案はどうでしょう。", "それでいこう", "2行で省略するようにしました。"),
    ("予定の削除は左スワイプにして、取り消しのトーストを出すのがいいと思います。", "いいね、そっちで", "左スワイプで削除、取り消しも出るようにしました。"),
    ("旅程の色分けは出発日の月ごとにする案があります。", "OK それ採用", "月ごとに色を分けました。"),
    ("ホテルの予約番号は長押しでコピーできるようにするのがおすすめです。", "それでいこう", "長押しでコピーできるようにしました。"),
    ("空の旅程には「最初の予定を追加」ボタンだけ出すのがいいと思います。", "いいね、そっちで", "空のときはボタンだけ出すようにしました。"),
    ("時刻の入力は 15 分刻みのピッカーにする案です。", "OK それ採用", "15 分刻みにしました。"),
    ("設定画面の項目は、よく使う順に並べ替える案がいいと思います。", "それでいこう", "よく使う順に並べ替えました。"),
    ("旅程の共有リンクは 30 日で切れるようにするのがおすすめです。", "いいね、そっちで", "共有リンクを 30 日で切れるようにしました。"),
    ("移動時間は手入力にして、自動計算は後回しにする案があります。", "OK それ採用", "移動時間を手入力にしました。"),
    ("予定のメモ欄は3行まで表示して、タップで全部出すのがいいと思います。", "それでいこう", "メモ欄を3行表示にしました。"),
    ("テーマ色は青系の1案に絞るのがおすすめです。", "いいね、そっちで", "テーマ色を青系にしました。"),
    ("過去の旅程は一覧の下に折りたたむ案がいいと思います。", "OK それ採用", "過去の旅程を折りたたむようにしました。"),
    ("アイコンは線のアイコンで揃えるのがおすすめです。", "それでいこう", "線のアイコンに揃えました。"),
    ("検索は旅程名と場所名の両方を対象にする案です。", "いいね、そっちで", "両方を検索するようにしました。"),
    ("予定の並び替えはドラッグにして、時刻は自動で直さない案がいいと思います。", "OK それ採用", "ドラッグで並び替えられるようにしました。"),
]

TABI_REQUESTS = [
    ("旅程の並び替えできるようにして", "旅程を出発日順と作成日順で並べ替えられるようにしました。"),
    ("ホテルの予約番号を保存したい", "予定に予約番号の欄を足しました。"),
    ("予定にメモ書けるようにして", "予定にメモ欄を足しました。"),
    ("旅程をコピーできるようにして", "旅程の複製ボタンを足しました。"),
    ("移動の予定に所要時間を入れたい", "移動の予定に所要時間の欄を足しました。"),
    ("README 更新して", "README に起動方法と画面の説明を書きました。"),
    ("テスト落ちてる", "日付の比較が UTC になっていたので直しました。テストは通っています。"),
    ("ビルド遅いの何とかならない？", "依存の重複を減らして、ビルドを 40 秒から 25 秒にしました。"),
    ("予定の時刻が 24 時を超えるとおかしい", "翌日扱いになるように直しました。"),
    ("旅程の名前変えられるようにして", "旅程名を編集できるようにしました。"),
    ("アプリのアイコン仮のやつ入れといて", "仮のアイコンを入れました。"),
    ("lint の警告消して", "未使用の import を消しました。"),
    ("予定に URL 貼れるようにして", "予定に URL の欄を足しました。"),
    ("旅程の合計日数を出して", "旅程詳細の上に日数を出しました。"),
    ("エラーメッセージもう少しやさしく", "エラーメッセージを言い換えました。"),
    ("ログイン画面のレイアウト崩れてる", "小さい画面で崩れていたのを直しました。"),
]

MAP_TASKS = [
    ("地図のピン見づらい", ["地図のピンの色を予定の種類ごとに変えました。", "地図のピンに影をつけて見やすくしました。"]),
    ("地図の最初の表示、もうちょい寄って", ["地図の初期ズームを 13 にしました。"]),
    ("地図が出ないことがある", ["Leaflet のタイル読み込みでエラーが出ていたので、タイルの URL を https に直しました。", "Leaflet のタイル読み込みの失敗時に再読み込みするようにしました。"]),
    ("地図重い", ["地図の表示が遅いので、Leaflet のタイルのキャッシュ期間を見直しました。", "地図のマーカーを必要な分だけ描くようにしました。"]),
    ("現在地わかりにくい", ["地図上の現在地マーカーを少し大きくしました。"]),
    ("バンドル大きくない？", ["Leaflet のプラグインを1つ削除して、バンドルを軽くしました。"]),
    ("地図の縮尺どこ？", ["地図の縮尺表示を右下に移しました。"]),
    ("地図触るとリストまで動く", ["地図をスクロールしたときに予定リストが動かないようにしました。"]),
    ("ライブラリ更新しといて", ["ライブラリを更新しました。Leaflet は 1.9.4 にしています。", "地図まわりの型定義も更新しました。"]),
    ("地図のボタン押しにくい", ["地図の操作ボタンを大きくしました。", "Leaflet の操作ボタンの余白を広げました。"]),
    ("地図の上で長押ししたら予定足せるようにして", ["地図の長押しで予定を追加できるようにしました。", "Leaflet のイベントで長押しを拾っています。"]),
    ("地図の色、テーマに合わせて", ["地図の上に重ねる線の色をテーマ色に合わせました。"]),
    ("地図のルート線太すぎ", ["地図のルート線を細くしました。"]),
    ("オフラインのとき地図どうなる？", ["Leaflet のタイルは表示したことのある範囲だけ端末に残っています。", "地図はキャッシュの範囲だけ表示されます。"]),
]

HOME_PHOTO_NOISE = [
    "ホーム画面の旅程カード {n} 枚目の表示を直しました。",
    "起動時の読み込みを {n}0 ms 短くしました。",
    "写真のサムネイルを {n} 枚分作り直しました。",
    "写真の保存処理は変えていません。",
    "クラウドとの同期の間隔は変えていません。",
    "ホーム画面の並び順はそのままです。",
]

SPEC_HEAD = """旅アプリの仕様メモ、これで進めて。

## 対象
国内旅行を自分で計画する人向け。旅程を日ごとに組み立てて、当日はそれを見ながら動く使い方を想定。家族や友だちとの旅行で、ひとりが計画して他の人は見るだけ、という形がいちばん多いはず。

## 画面
- 旅程一覧：作った旅程を出発日の近い順に並べる。過去の旅程は下にまとめて折りたたむ。
- 旅程詳細：日ごとのタブ。各日は時刻順の予定リストと地図。予定をタップすると詳細が開く。
- 予定の編集：名前、時刻、場所、メモ、予約番号。場所は検索か地図の長押しで決める。
- 設定：表示名、テーマ、データの書き出し。

## 予定の扱い
予定は時刻の順に並べる。時刻を決めていない予定はその日の末尾に置く。移動の予定は出発地と到着地を持ち、所要時間を手で入れられるようにする。予約番号は長押しでコピーできるようにする。予定の色は種類（移動、食事、宿、観光）で分ける。種類を決めていない予定は灰色。

## 旅程の作成
旅程は名前と出発日と日数だけで作れるようにする。あとから日数を減らしたときは、はみ出した日の予定を消さずに「未定」の日にまとめる。旅程の複製もできるようにする。テンプレートは作らない。

## 地図
各日の地図には、その日の予定の場所をピンで出して、移動の予定は線でつなぐ。ピンの色は予定の色と同じにする。地図を長押しするとその場所で予定を追加できる。現在地は旅行の当日だけ出す。

## 検索
旅程名と場所名で検索できる。検索結果は旅程ごとにまとめて、出発日の新しい順に並べる。場所の検索は地図のサービスの検索を使い、候補を5件まで出す。

## 入力
時刻は15分刻みのピッカーで入れる。場所は検索か地図の長押しで決めて、手入力の住所は受け付けない。メモは自由に書けて、URL は自動でリンクにする。

## 予約の情報
宿や交通の予約番号、チェックインの時刻、問い合わせ先の電話番号を予定ごとに持てる。電話番号はタップで電話をかけられるようにする。予約の確認メールを貼り付けたら、そこから番号と時刻を拾えるとうれしいが、最初の版ではやらない。

## 並べ方
旅程一覧は出発日の近い順。日付が同じ旅程は作った順。過去の旅程は新しい順に折りたたむ。並べ方の設定は作らない。

## 色
色はテーマに合わせて青系にする。予定の種類の色だけは見分けやすいように別に決める。

## データ
データはアカウントに紐づけてサーバーにも置く。端末を替えても旅程が引き継げるようにするため。ただし写真は端末の中だけに置く。

## 同期
"""

SPEC_L1 = "地下や山の中みたいに通信できない場所では、直近7日分の旅程だけ見られるようにして、編集は通信が戻って同期が終わってからにする。"

SPEC_MIDDLE = """古い旅程まで全部端末に持つと容量を食うので、それより前の旅程は通信できるときだけ開ける形でいい。同期の競合は、あとから保存した方を残して、消えた側の内容は履歴に残す。同期は旅程を開いたときと、保存したときに行う。バックグラウンドでの同期は電池を食うのでしない。同期に失敗したときは画面の上に小さく出して、もう一度試すボタンを置く。

## お知らせ
"""

SPEC_L2 = "通知は朝8時と出発3時間前だけにする。それ以外の通知は出さない。"

SPEC_TAIL = """旅行中に何度も鳴ると嫌なので、ここは増やさない。

## 当日の画面
旅行の当日は、旅程詳細をその日のタブで開く。今の時刻に近い予定を上に寄せて、終わった予定は薄く表示する。次の予定までの残り時間を画面の上に出す。

## 天気
天気は出さない。調べたいときは天気のアプリを使えばいい。

## アカウント
アカウントはメールアドレスとパスワードで作る。SNS でのログインは最初の版では入れない。パスワードを忘れたときはメールで再設定のリンクを送る。退会したらデータは30日後に消す。

## 写真
写真は予定ごとに付けられる。写真は端末の中だけに置いて、サーバーには上げない。旅程を共有しても写真は共有されない。

## 費用
費用の記録はしない。割り勘の計算もしない。必要になったら別のアプリを使う。

## 端末
スマホだけを対象にする。タブレットでは大きくしたスマホの画面のまま。PC 版は作らないが、共有リンクはブラウザで見られるようにする。

## 開発
画面ができたものから順に、自分の旅行で使って確かめる。テストは予定の並べ替えと同期の部分だけはしっかり書く。デザインは後で整えるので、最初は見た目より動くことを優先する。

## 名前
アプリの名前はまだ決めない。仮に「たびメモ」と呼ぶ。アイコンも仮のものでいい。リリースの前に名前とアイコンをまとめて決める。

## 問い合わせ
問い合わせはメールだけで受ける。アプリの中にフォームは作らない。よくある質問は、たまってきたらページを作る。

## 速さ
旅程一覧は1秒以内に出す。予定が100件ある旅程でも、日ごとのタブの切り替えは待たせない。

## 共有
旅程は URL で共有できる。共有された側はログインなしで閲覧だけできる。共有リンクは作った人があとから止められるようにする。共有した相手が編集したいときは、アカウントを作ってもらって招待する形にする。

## 書き出し
旅程はテキストと PDF で書き出せる。PDF は1日1ページで、予定のリストと簡単な地図を載せる。テキストは予定を時刻順に並べただけのもの。

## その他
ダークモード対応は後回し。多言語対応も今はしない。課金はしない。広告も入れない。アプリ内のヘルプは最低限にして、わからないところは画面の中の説明文で補う。アイコンとスクリーンショットはリリースの前にまとめて作る。最初のリリースは自分と家族で使ってみて、使いにくいところを直してから公開する。"""


def spec_text():
    text = SPEC_HEAD + SPEC_L1 + SPEC_MIDDLE + SPEC_L2 + SPEC_TAIL
    first, second = text.index(SPEC_L1), text.index(SPEC_L2)
    # The middle a reader of the head and the tail of 1800 characters never sees.
    if not (900 < first and second + len(SPEC_L2) < len(text) - 900 and 2400 < len(text) < 3200):
        raise SystemExit(f"the spec's key points are not in its middle: {first}, {second}, {len(text)}")
    return text


def tabi_sessions(rng):
    sessions = []
    acks = list(ACK_PAIRS)
    rng.shuffle(acks)
    requests = list(TABI_REQUESTS)
    map_tasks = list(MAP_TASKS)
    rng.shuffle(map_tasks)
    key_days = {"2026-07-08": "k1", "2026-07-22": "k2", "2026-08-05": "k3", "2026-08-20": "spec"}
    days = sorted(set(dates(rng, (7, 1), (10, 15), 26)) | set(key_days))
    for index, day in enumerate(days):
        session = new_session("claude", TABI, day, f"s{index:02d}")
        write = Writer(session, f"{rng.randint(9, 20):02d}:{rng.choice([0, 10, 20, 30]):02d}")
        steps = rng.randint(3, 5)
        for step in range(steps):
            if step == 1 and key_days.get(day) == "k1":
                write.you("地図まわり、どのライブラリ使うのがいいと思う？オフラインでも見られるといい")
                write.agent("候補は Leaflet、Mapbox GL JS、Google Maps の3つです。無料で使えて、タイルを端末に保存してオフライン表示もできる Leaflet をおすすめします。Mapbox は表現力は高いですが従量課金で、Google Maps も利用量に応じて料金がかかります。", "k1_offer")
                write.you("それでいこう", "k1_ok")
                write.tool("Bash", "npm install leaflet", "added 1 package in 2s")
                write.agent("Leaflet を入れて、旅程画面に地図を表示しました。")
                continue
            if step == 1 and key_days.get(day) == "k2":
                write.you("アプリ開いたとき最初の画面どうする？")
                write.agent("毎回ホーム画面（旅程一覧）を出す案と、前回開いていた旅程をそのまま開く案があります。旅行中は同じ旅程を何度も見るので、前回の旅程を開く方がいいと思います。", "k2_offer")
                write.you("いいね、そっちで", "k2_ok")
                write.agent("起動したときに前回開いていた旅程を開くようにしました。")
                continue
            if step == 1 and key_days.get(day) == "k3":
                write.you("旅の写真ってどこに置く？")
                write.agent("クラウドに上げる案もありますが、プライバシーと通信量を考えると、写真は端末内だけに保存してクラウドには上げない方式をおすすめします。", "k3_offer")
                write.you("OK それ採用", "k3_ok")
                write.agent("写真は端末内だけに保存するようにしました。")
                continue
            if step == 0 and key_days.get(day) == "spec":
                write.you(spec_text(), "l_spec")
                write.agent("仕様メモを確認しました。画面の構成から順に実装します。")
                continue
            roll = rng.random()
            if roll < 0.35 and acks:
                proposal, ack, done = acks.pop()
                write.you(rng.choice(["ここどうするのがいい？", "おすすめある？", "どっちがいいかな"]))
                write.agent(proposal)
                write.you(ack)
                write.agent(done)
            elif roll < 0.65 and map_tasks and day > "2026-07-08":
                request, replies = map_tasks.pop()
                write.you(request)
                for reply in replies:
                    write.agent(reply, step=rng.randint(1, 3))
            else:
                request, done = rng.choice(requests)
                write.you(request)
                write.agent(done, step=rng.randint(1, 4))
                if rng.random() < 0.4:
                    write.you(rng.choice(["ありがと", "ok", "助かる"]))
        sessions.append(session)
    # Whatever was not used goes into a last session, so the counts do not depend on luck.
    tail = new_session("claude", TABI, "2026-10-16", "tail")
    write = Writer(tail, "09:00")
    for proposal, ack, done in acks:
        write.you("ここどうするのがいい？")
        write.agent(proposal)
        write.you(ack)
        write.agent(done)
    for request, replies in map_tasks:
        write.you(request)
        for reply in replies:
            write.agent(reply)
    sessions.append(tail)
    # The agents' newer records about the map, holding the words the question may be searched
    # with: the July proposal is far from the newest of them.
    more = new_session("claude", TABI, "2026-10-17", "maps")
    write = Writer(more, "08:00")
    for number in range(30):
        write.you(rng.choice(["地図まわり見て", "地図のとこ直して", "地図の件つづき"]))
        write.agent(f"地図の表示を確認しました。Leaflet のレイヤー {number + 1} 番目の描画順を直しています。", step=2)
        write.agent("地図ライブラリ（Leaflet）の設定はそのままです。", step=1)
        write.agent(rng.choice(HOME_PHOTO_NOISE).format(n=number + 2), step=1)
    sessions.append(more)
    return sessions


# ---------------------------------------------------------------------------------------
# atelier: many design remarks, and three rules restated

RULE_A = [  # 色は3色まで
    "色は3色までにして",
    "色増やさないで、3色で収めたい",
    "またアクセント色が増えてる。3色ルールね",
    "4色目はいらない",
    "使う色はベースと文字とアクセントの3つだけにして",
    "色数増やすのやめよう、3色まで",
    "ここも色が多い。3色以内で",
    "新しい色は作らないで、今ある3色で",
    "色は3色って前も言ったよね",
]
RULE_B = [  # フォントは1種類
    "フォントは1種類で統一して",
    "見出しだけ別のフォントにしないで",
    "フォント混ぜないで",
    "書体は Noto Sans JP だけでいい",
    "フォントは増やさない方針で",
    "このバナーだけ明朝になってる、1種類にして",
    "フォントは1つ",
    "差はウェイトでつけて、書体は変えない",
]
RULE_C = [  # 余白は詰めない
    "余白は詰めないで",
    "詰め込みすぎ。もっと余白とって",
    "情報が多くても余白は削らない",
    "余白を削って収めるのはやめて",
    "ゆったりさせたいから余白詰めないで",
    "スマホでも余白は詰めない",
    "余白はケチらない方針で",
]
AGENT_ACKS = {
    "a": ["3色に収まるように色を直しました。", "アクセント色を既存の色に戻しました。"],
    "b": ["フォントを Noto Sans JP に揃えました。", "別のフォントを外しました。"],
    "c": ["余白を広げました。", "余白を元の広さに戻しました。"],
}
PAGES = ["トップページ", "作品一覧", "作品詳細", "プロフィール", "お問い合わせ", "ブログ一覧", "ブログ記事", "フッター"]
TARGETS = ["見出し", "本文", "ボタン", "カード", "画像", "リンク", "ナビ", "アイコン", "フォーム", "区切り線"]
CHANGES = ["を少し大きくして", "を少し小さくして", "の影を薄くして", "の角丸をやめて", "を中央寄せにして",
           "を左寄せにして", "の線を細くして", "を太字にして", "の間隔を揃えて", "の位置を少し下げて",
           "の色を紺にして", "の色を濃いグレーにして", "をアクセント色にして", "をモノクロにして",
           "のホバーで下線を出して", "を2列にして", "を3列にして", "の行間を少し広げて"]
MARGIN_ONE_OFFS = ["カードの内側の余白を 24px にして", "ヘッダーの上下の余白を 16px に", "画像と本文の間の余白を少しだけ広げて",
                   "フォームの項目間の余白を揃えて", "フッターの余白を左右だけ広げて", "見出しの下の余白を 12px にして"]
ATELIER_DONE = ["直しました。", "変更しました。", "反映しました。", "そのようにしました。"]
ATELIER_SMALL = ["いい感じ", "ありがとう", "OK", "確認した", "うん"]


def atelier_sessions(rng):
    days = dates(rng, (6, 1), (10, 15), 64)
    # Which sessions restate which rule: spread over the whole span, the same every time.
    placements = {}
    for rule, lines in (("a", RULE_A), ("b", RULE_B), ("c", RULE_C)):
        picks = sorted(rng.sample(range(len(days)), len(lines)))
        for number, (session_index, line) in enumerate(zip(picks, lines), 1):
            placements.setdefault(session_index, []).append((rule, number, line))
    combos = [f"{page}の{target}{change}" for page in PAGES for target in TARGETS for change in CHANGES]
    rng.shuffle(combos)
    sessions = []
    for index, day in enumerate(days):
        session = new_session("claude", ATELIER, day, f"s{index:02d}")
        write = Writer(session, f"{rng.randint(8, 13):02d}:{rng.choice([0, 15, 30, 45]):02d}")
        restatements = list(placements.get(index, []))
        remarks = rng.randint(8, 12)
        slots = sorted(rng.sample(range(remarks), min(len(restatements), remarks)))
        for remark in range(remarks):
            if restatements and slots and remark == slots[0]:
                slots.pop(0)
                rule, number, line = restatements.pop(0)
                write.you(line, f"m{rule}{number}")
                write.agent(rng.choice(AGENT_ACKS[rule]), step=rng.randint(1, 3))
                continue
            roll = rng.random()
            if roll < 0.06:
                write.you(rng.choice(MARGIN_ONE_OFFS))
            elif roll < 0.12:
                write.you(rng.choice(ATELIER_SMALL))
                continue
            else:
                write.you(combos.pop())
            write.agent(rng.choice(ATELIER_DONE), step=rng.randint(1, 3))
        for rule, number, line in restatements:
            write.you(line, f"m{rule}{number}")
            write.agent(rng.choice(AGENT_ACKS[rule]))
        sessions.append(session)
    return sessions


# ---------------------------------------------------------------------------------------
# sensor-hub: old causes under newer agent records with the same words

SENSOR_NOISE = [
    ("トピック名揃えて", ["MQTT のトピック名を sensors/<id>/temp の形に揃えました。", "MQTT の購読側もトピック名を合わせました。"]),
    ("QoS 上げといて", ["MQTT の QoS を 1 にしました。keepalive の設定は変えていません。"]),
    ("API 遅いときにずっと待つのやめて", ["HTTP のタイムアウトを 10 秒にしました。", "タイムアウトしたときはエラーを記録して次に進みます。"]),
    ("起動ログもっと出して", ["起動スクリプトに --verbose を足しました。起動時の設定を全部ログに出します。"]),
    ("校正画面の文言直して", ["校正画面の文言を直しました。", "校正の手順を README にも書きました。"]),
    ("キャッシュ古いの残ってない？", ["キャッシュの有効期限を 5 分にしました。", "起動時に古いキャッシュを消すようにしました。"]),
    ("切れたらすぐつなぎ直して", ["MQTT の接続が切れたときの再接続間隔を 5 秒にしました。", "再接続の回数をログに出すようにしました。"]),
    ("ブローカーのログ肥大してる", ["ブローカーのログローテーションを設定しました。", "ブローカーのログは7日分残します。"]),
    ("ブローカー再起動の手順まとめて", ["ブローカーの再起動手順を README に書きました。MQTT のクライアントは自動でつなぎ直します。"]),
    ("センサー増やしたい", ["センサーの登録を設定ファイルからできるようにしました。", "MQTT のトピックは登録したセンサーの分だけ購読します。"]),
    ("温度の単位、設定で変えられるように", ["温度の単位を設定で切り替えられるようにしました。"]),
    ("ダッシュボードの更新間隔は？", ["ダッシュボードは 5 秒ごとに更新しています。MQTT のメッセージが来たときにも更新します。"]),
    ("テスト落ちてる", ["起動時の設定読み込みのテストが、校正データのパスを固定していたので直しました。"]),
    ("keepalive って今いくつ？", ["keepalive は今の設定のままです。MQTT の設定ファイルで変えられます。"]),
    ("接続数の上限ある？", ["ブローカーの接続数の上限は 100 にしています。"]),
    ("データ取り込み遅い", ["取り込みをまとめて書き込むようにして、読み込みの回数を減らしました。", "キャッシュを使うようにしたので、同じ問い合わせは速くなります。"]),
    ("Docker で起動できるように", ["Dockerfile を足しました。起動は docker compose up でできます。", "ブローカーも compose で一緒に起動します。"]),
    ("エラー時に通知ほしい", ["接続のタイムアウトが3回続いたら通知するようにしました。"]),
    ("接続の設定見直して", ["MQTT の keepalive と再接続の設定を確認しました。変更はしていません。"]),
    ("ping 送ってる？", ["MQTT の keepalive の ping は正常に送られています。"]),
    ("ダッシュボードが真っ白", ["原因は API の URL の設定漏れでした。直しました。"]),
    ("グラフがずれる", ["原因はタイムゾーンの扱いでした。UTC で揃えました。"]),
    ("校正し直したい", ["校正データを作り直す手順を足しました。", "校正の結果をグラフで見られるようにしました。"]),
    ("校正の値おかしい", ["原因は校正の係数の符号が逆だったことでした。直しました。"]),
    ("設定の読み込み遅い", ["設定ファイルの読み込みを1回にまとめました。キャッシュも効くようにしています。"]),
]
SUMMARY_PHRASES = ["MQTT の設定は変更なし", "keepalive は変更なし", "ブローカーの状態は正常",
                   "接続のタイムアウトは変更なし", "起動時間は計測していません", "校正データは触っていません",
                   "キャッシュはそのまま", "読み込みの処理は変更なし", "原因の調査は不要でした"]
SENSOR_SMALL = ["直った", "ありがとう", "ok", "助かる", "いいね"]


def sensor_sessions(rng):
    sessions = []
    # The start-up delay, in July.
    session = new_session("codex", SENSOR, "2026-07-15", "slow-start")
    write = Writer(session, "10:00")
    write.you("起動が遅い。立ち上がるまで10秒くらいかかる", "n2_report")
    write.agent("起動処理のどこに時間がかかっているか計測します。")
    write.tool("shell", "python -X importtime -m hub 2>&1 | tail -3", "import time: 9812345 | hub.calibration\nimport time:   20311 | hub.mqtt")
    write.agent("起動時に校正データを読み込むところで時間がかかっています。中身を確認します。")
    write.tool("shell", "ls -la data/calibration.json", "-rw-r--r-- 1 pi pi 41943040 Jul 15 09:58 data/calibration.json")
    write.agent("原因は起動時に校正データ（calibration.json、約40MB）を毎回全部読み込んでいたことでした。初回だけ読み込んで結果をキャッシュするようにしたので、2回目以降の起動は1秒ほどになります。", "n2_cause", step=3)
    write.you("速くなった、ありがとう", "n2_fixed", step=5)
    write.agent("よかったです。")
    sessions.append(session)
    # The values that stop arriving, in August.
    session = new_session("codex", SENSOR, "2026-08-04", "values-stop")
    write = Writer(session, "21:00")
    write.you("センサーの値が途中で来なくなる。数分は来るけどそのうち止まる", "n1_report")
    write.tool("shell", "journalctl -u sensor-hub --since today | tail -2", "mqtt: connection lost\nmqtt: reconnecting")
    write.agent("ログを見ると、接続が数分おきに切れています。ブローカーの設定を確認します。")
    write.tool("shell", "grep keepalive /etc/mosquitto/mosquitto.conf", "max_keepalive 30")
    write.agent("原因がわかりました。クライアントの MQTT keepalive が 60 秒で、ブローカー側の max_keepalive 30 秒より長いため、ブローカーが接続を切っていました。keepalive を 20 秒にしました。", "n1_cause", step=3)
    write.you("直った", "n1_fixed", step=10)
    write.agent("よかったです。しばらく様子を見てください。")
    sessions.append(session)
    # Earlier and later everyday work, most of it later, with the same words.
    days = dates(rng, (6, 20), (7, 10), 6) + dates(rng, (8, 10), (10, 15), 70)
    for index, day in enumerate(days):
        session = new_session("codex", SENSOR, day, f"s{index:02d}")
        write = Writer(session, f"{rng.randint(9, 19):02d}:{rng.choice([0, 20, 40]):02d}")
        for _ in range(rng.randint(2, 4)):
            request, replies = rng.choice(SENSOR_NOISE)
            write.you(request)
            for reply in replies:
                write.agent(reply, step=rng.randint(1, 3))
            if rng.random() < 0.35:
                write.you(rng.choice(SENSOR_SMALL))
        if day > "2026-08-04" and rng.random() < 0.8:
            write.agent("今日の変更のまとめ: " + "、".join(rng.sample(SUMMARY_PHRASES, 4)) + "。")
        sessions.append(session)
    return sessions


# Agreements where the person never names the matter, to proposals that newer agent records
# holding the same words bury: (date, start, the person's word before, the proposal, its key,
# the agreement, its key, what the agent did).
HARD_ACKS = [
    ("2026-09-10", "14:00", "続けて",
     "画面の実装が終わりました。ひとつ相談です。予定の時刻は、端末の時間帯で出す方式と、旅行先の時間帯で出す方式があります。海外にも持っていくなら、旅行先の時間帯で出す方がずれなくていいと思います。",
     "k4_offer", "そうしよう", "k4_ok", "予定の時刻を旅行先の時間帯で出すようにしました。"),
    ("2026-09-18", "20:30", "次お願い",
     "書き出しは CSV と PDF の両方を予定していますが、どちらから作りましょうか。印刷して持ち歩く使い方が多いなら、PDF からがいいと思います。",
     "k5_offer", "任せる、それで", "k5_ok", "PDF の書き出しから作りました。CSV は次に作ります。"),
    ("2026-09-25", "10:15", "で、次は？",
     "旅程を消したときの扱いですが、すぐには消さずにゴミ箱に14日間残して、そのあいだは戻せるようにするのがいいと思います。",
     "k6_offer", "うん、その方向で", "k6_ok", "旅程を消したら14日間ゴミ箱に残るようにしました。"),
]
GENERIC_ACKS = ["そうしよう", "任せる、それで", "うん、その方向で"]
GENERIC_WORDS = ["続けて", "次お願い", "で、次は？", "これも見て", "そのまま進めて"]
UNRELATED_PROPOSALS = [
    ("ボタンの文字は「保存」と「保存する」のどちらにしましょう。短い方が収まりがいいと思います。", "ボタンの文字を「保存」にしました。"),
    ("一覧の読み込み中は、くるくる回る表示より、枠だけ先に出す方が落ち着くと思います。", "読み込み中は枠を先に出すようにしました。"),
    ("設定の項目名は「表示名」より「名前」の方がわかりやすいと思います。", "項目名を「名前」にしました。"),
    ("エラーの記録は端末に7日分だけ残す形でいいと思います。", "エラーの記録を7日分残すようにしました。"),
    ("アプリの色の濃さは、少し落ち着いた青にするのがいいと思います。", "青を少し落ち着いた色にしました。"),
    ("並び替えのボタンは右上に置くのが自然だと思います。", "並び替えのボタンを右上に置きました。"),
]
LATE_NOISE = [
    "予定の時刻の表示と時間帯の設定を確認しました（旅行先 {n} 件、タイムゾーンは変更なし）。",
    "書き出しの PDF と CSV を確認しました（{n} 件目、レイアウトは変更なし）。",
    "削除まわりを確認しました。ゴミ箱の表示と旅程を消す操作は変更なし（{n} 件目）。",
]


def hard_short_reply_sessions():
    """The sessions of the harder agreements, and the newer records that bury the proposals.
    A random source of their own keeps the other sessions as they were."""
    rng = random.Random(4646)
    sessions = []
    for index, (day, start, before, offer, offer_key, ack, ack_key, done) in enumerate(HARD_ACKS):
        session = new_session("claude", TABI, day, f"hard{index}")
        write = Writer(session, start)
        for _ in range(2):
            request, reply = rng.choice(TABI_REQUESTS)
            write.you(request)
            write.agent(reply, step=rng.randint(1, 3))
        write.you(before)
        write.agent(offer, offer_key, step=3)
        write.you(ack, ack_key)
        write.agent(done, step=2)
        sessions.append(session)
    # The same agreements to other things, so that the words alone do not tell which.
    for index, day in enumerate(["2026-09-05", "2026-09-14", "2026-09-29", "2026-10-06", "2026-10-11"]):
        session = new_session("claude", TABI, day, f"acks{index}")
        write = Writer(session, f"{rng.randint(9, 18):02d}:00")
        for _ in range(3):
            proposal, did = rng.choice(UNRELATED_PROPOSALS)
            write.you(rng.choice(GENERIC_WORDS))
            write.agent(proposal, step=2)
            write.you(rng.choice(GENERIC_ACKS))
            write.agent(did, step=2)
        sessions.append(session)
    for index, day in enumerate(["2026-10-18", "2026-10-19"]):
        session = new_session("claude", TABI, day, f"late{index}")
        write = Writer(session, "08:00")
        for number in range(30):
            write.you(rng.choice(GENERIC_WORDS), step=3)
            for line in LATE_NOISE:
                write.agent(line.format(n=index * 30 + number + 1), step=1)
        sessions.append(session)
    return sessions


def sessions(seed=20261020):
    """Every stress session, the same every time."""
    rng = random.Random(seed)
    made = tabi_sessions(rng) + atelier_sessions(rng) + sensor_sessions(rng)
    return made + hard_short_reply_sessions()


if __name__ == "__main__":
    import collections
    made = sessions()
    count = collections.Counter()
    for session in made:
        for record in session["records"]:
            count[(session["project"], record["kind"])] += 1
    for (project, kind), number in sorted(count.items()):
        print(f"{project:12} {kind:6} {number}")
    keys = sorted(record["key"] for session in made for record in session["records"] if record["key"])
    print(len(made), "sessions;", len(keys), "keys:", " ".join(keys))
