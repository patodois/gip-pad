// Lists the gamepads SDL3 sees and prints their button and axis events for a few seconds.
#include <SDL3/SDL.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 10;
    if (!SDL_Init(SDL_INIT_GAMEPAD)) {
        printf("SDL_Init failed: %s\n", SDL_GetError());
        return 1;
    }
    Uint64 end = SDL_GetTicks() + (Uint64)seconds * 1000;
    SDL_Event e;
    while (SDL_GetTicks() < end) {
        while (SDL_PollEvent(&e)) {
            switch (e.type) {
            case SDL_EVENT_GAMEPAD_ADDED: {
                SDL_Gamepad *g = SDL_OpenGamepad(e.gdevice.which);
                printf("added: \"%s\" type %s vid %04x pid %04x path %s\n", SDL_GetGamepadName(g),
                       SDL_GetGamepadStringForType(SDL_GetGamepadType(g)), SDL_GetGamepadVendor(g),
                       SDL_GetGamepadProduct(g), SDL_GetGamepadPath(g) ? SDL_GetGamepadPath(g) : "-");
                printf("  rumble: %s\n", SDL_RumbleGamepad(g, 0x6000, 0x6000, 300) ? "ok" : SDL_GetError());
                break;
            }
            case SDL_EVENT_GAMEPAD_REMOVED:
                printf("removed\n");
                break;
            case SDL_EVENT_GAMEPAD_BUTTON_DOWN:
                printf("button %s\n", SDL_GetGamepadStringForButton((SDL_GamepadButton)e.gbutton.button));
                break;
            case SDL_EVENT_GAMEPAD_AXIS_MOTION:
                if (e.gaxis.value > 30000 || e.gaxis.value < -30000)
                    printf("axis %s %d\n", SDL_GetGamepadStringForAxis((SDL_GamepadAxis)e.gaxis.axis), e.gaxis.value);
                break;
            }
            fflush(stdout);
        }
        SDL_Delay(5);
    }
    SDL_Quit();
    return 0;
}
