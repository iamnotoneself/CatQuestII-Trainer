@echo off
rem 猫咪斗恶龙2 学习用修改器 启动脚本
cd /d "%~dp0"
where pythonw >nul 2>nul
if %errorlevel%==0 (
    start "" pythonw catquest_trainer.py
) else (
    echo 未找到 pythonw，改用 python 启动...
    python catquest_trainer.py
    pause
)
